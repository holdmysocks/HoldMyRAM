//! User configuration: TOML on disk, atomic saves, tolerant loading, export/import.

use crate::admin::rules;
use crate::profiles::Builtin;
use crate::types::{AddonRef, Selection, SelectionKind};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_HOTKEY: &str = "Ctrl+Alt+R";
pub const DEFAULT_GRACE_SECONDS: u64 = 5;
/// Upper bound accepted for the grace period.
pub const MAX_GRACE_SECONDS: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
    /// Follow the Windows light/dark setting.
    System,
}

/// A user profile (editable). The built-in Recommended profiles are not stored here; see
/// `profiles::Builtin`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub selections: Vec<Selection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub hotkey: String,
    pub theme: Theme,
    pub grace_seconds: u64,
    /// Selections used when no profile is active.
    pub selections: Vec<Selection>,
    pub profiles: Vec<Profile>,
    /// A user profile name, a built-in profile name ("Recommended", "Recommended+"), or
    /// None for the top-level selections. A field missing from an existing file loads as
    /// None (keeps old behaviour); a brand-new config (no file) starts at "Recommended".
    #[serde(default)]
    pub active_profile: Option<String>,
    /// The first-run tutorial was shown (or skipped). See `load_from` for the upgrade rule.
    #[serde(default)]
    pub tutorial_seen: bool,
    /// Optional ("add-on") items of the built-in Recommended profiles the user turned on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommended_addons: Vec<AddonRef>,
    /// Whole add-on groups ("optional", "optin") of the built-in profiles turned on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommended_addon_groups: Vec<String>,
    /// Check GitHub for a newer release about a minute after start (and when the window
    /// opens, at most every 6 hours). Missing in old files: true.
    #[serde(default = "yes")]
    pub auto_check_updates: bool,
    /// Download and verify a newer release in the background. Installing always needs a
    /// click. Missing in old files: true.
    #[serde(default = "yes")]
    pub auto_download_updates: bool,
}

fn yes() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Config {
            hotkey: DEFAULT_HOTKEY.to_string(),
            theme: Theme::default(),
            grace_seconds: DEFAULT_GRACE_SECONDS,
            selections: Vec::new(),
            profiles: Vec::new(),
            active_profile: Some(crate::profiles::RECOMMENDED.to_string()),
            tutorial_seen: false,
            recommended_addons: Vec::new(),
            recommended_addon_groups: Vec::new(),
            auto_check_updates: true,
            auto_download_updates: true,
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(String),
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "I/O error: {e}"),
            ConfigError::Parse(e) => write!(f, "could not parse config: {e}"),
            ConfigError::Invalid(e) => write!(f, "invalid config: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

/// Result of loading: always a usable config, plus a warning if something was wrong.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub warning: Option<String>,
}

/// `%APPDATA%\HoldMyRAM` (falls back to `.\HoldMyRAM` if APPDATA is unset).
pub fn app_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("HoldMyRAM")
}

/// Default config file location.
pub fn config_path() -> PathBuf {
    app_dir().join("config.toml")
}

fn is_valid_exe_name(name: &str) -> bool {
    let t = name.trim();
    !t.is_empty() && !t.contains(['\\', '/', ':', '\0'])
}

const ANTI_CHEAT_SERVICES: &[&str] =
    &["vgc", "vgtray", "BEService", "BEDaisy", "EasyAntiCheat", "EasyAntiCheat_EOS", "EAAntiCheat.GameService", "FACEITService", "FACEITClient"];
const VENDOR_SERVICES: &[&str] = &[
    "NvContainerLocalSystem",
    "AsusOptimization",
    "AsusPTPService",
    "AsusAppService",
    "ASUSSystemAnalysis",
    "ASUSSystemDiagnosis",
];
const SECURITY_SERVICES: &[&str] = &["WinDefend", "MDCoreSvc", "WdNisSvc", "SecurityHealthService", "Sense", "wscsvc"];

/// Why HoldMyRAM will never change this service or task, in words for the UI, or None.
/// Uses the same hard-block lists as the elevated helper (`admin::rules`), which re-checks
/// every request on its own anyway. Processes are never blocked here (see plan.rs).
pub fn admin_block_reason(kind: SelectionKind, name: &str) -> Option<String> {
    match kind {
        SelectionKind::Process => None,
        SelectionKind::Service => {
            let hit = rules::never_touch_service(name)?;
            let is = |l: &[&str]| l.iter().any(|x| x.eq_ignore_ascii_case(hit));
            let what = if is(ANTI_CHEAT_SERVICES) {
                "Anti-cheat service"
            } else if is(VENDOR_SERVICES) {
                "Hardware vendor service this PC depends on"
            } else if is(SECURITY_SERVICES) {
                "Windows security service"
            } else {
                "Core Windows service"
            };
            Some(format!("{what}, cannot be changed (never-touch list: {hit})"))
        }
        SelectionKind::Task => rules::never_touch_task(name)
            .map(|f| format!("Protected Windows task folder, cannot be changed (never-touch list: {f})")),
    }
}

/// Shape checks: name syntax for its kind, and a method that fits the kind.
pub(crate) fn check_shape(s: &Selection) -> Result<(), String> {
    if !s.kind.allows(s.method) {
        let want = match s.kind {
            SelectionKind::Process => "close, suspend or kill",
            SelectionKind::Service => "stop",
            SelectionKind::Task => "disable",
        };
        let kind = format!("{:?}", s.kind).to_lowercase();
        let method = format!("{:?}", s.method).to_lowercase();
        return Err(format!("{:?} is a {kind}; its method must be {want}, not {method}", s.name));
    }
    match s.kind {
        SelectionKind::Process if !is_valid_exe_name(&s.name) => Err(format!("bad process name {:?}", s.name)),
        SelectionKind::Process => Ok(()),
        SelectionKind::Service => rules::validate_service_name(&s.name).map_err(|e| format!("{e}: {:?}", s.name)),
        SelectionKind::Task => rules::validate_task_path(&s.name).map_err(|e| format!("{e}: {:?}", s.name)),
    }
}

fn validate_selections(list: &[Selection], what: &str, blocked_too: bool) -> Result<(), ConfigError> {
    for s in list {
        check_shape(s).map_err(|e| ConfigError::Invalid(format!("{what}: {e}")))?;
        if blocked_too {
            if let Some(why) = admin_block_reason(s.kind, &s.name) {
                return Err(ConfigError::Invalid(format!("{what}: {:?}: {why}", s.name)));
            }
        }
    }
    Ok(())
}

impl Config {
    /// Check the config for values that would be unsafe or meaningless, including
    /// services and tasks on the never-touch lists. Used on save and import.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_inner(true)
    }

    /// Drop every selection on the never-touch lists (they can only have got there by
    /// hand-editing, or by the lists growing in a newer version). Returns what was dropped.
    fn strip_blocked(&mut self) -> Vec<String> {
        let mut dropped = Vec::new();
        let mut strip = |list: &mut Vec<Selection>| {
            list.retain(|s| match admin_block_reason(s.kind, &s.name) {
                Some(why) => {
                    dropped.push(format!("{} ({why})", s.name));
                    false
                }
                None => true,
            })
        };
        strip(&mut self.selections);
        for p in &mut self.profiles {
            strip(&mut p.selections);
        }
        dropped
    }

    fn validate_inner(&self, blocked_too: bool) -> Result<(), ConfigError> {
        if self.hotkey.trim().is_empty() {
            return Err(ConfigError::Invalid("hotkey is empty".into()));
        }
        if self.grace_seconds > MAX_GRACE_SECONDS {
            return Err(ConfigError::Invalid(format!(
                "grace_seconds {} exceeds {MAX_GRACE_SECONDS}",
                self.grace_seconds
            )));
        }
        validate_selections(&self.selections, "selections", blocked_too)?;
        for a in &self.recommended_addons {
            if a.name.trim().is_empty() || a.name.len() > 1024 || a.name.contains('\0') {
                return Err(ConfigError::Invalid(format!("recommended_addons: bad name {:?}", a.name)));
            }
        }
        for g in &self.recommended_addon_groups {
            if !crate::profiles::ADDON_GROUPS.iter().any(|x| x.eq_ignore_ascii_case(g.trim())) {
                return Err(ConfigError::Invalid(format!(
                    "recommended_addon_groups: unknown group {g:?} (expected {})",
                    crate::profiles::ADDON_GROUPS.join(" or ")
                )));
            }
        }
        let mut seen: Vec<String> = Vec::new();
        for p in &self.profiles {
            let n = p.name.trim();
            if n.is_empty() {
                return Err(ConfigError::Invalid("profile with empty name".into()));
            }
            if Builtin::from_name(n).is_some() {
                return Err(ConfigError::Invalid(format!(
                    "{n:?} is the name of a built-in profile; choose another name"
                )));
            }
            let lower = n.to_lowercase();
            if seen.contains(&lower) {
                return Err(ConfigError::Invalid(format!("duplicate profile {n:?}")));
            }
            seen.push(lower);
            validate_selections(&p.selections, &format!("profile {n:?}"), blocked_too)?;
        }
        if let Some(a) = &self.active_profile {
            if Builtin::from_name(a).is_none() && self.find_profile(a).is_none() {
                return Err(ConfigError::Invalid(format!(
                    "active profile {a:?} does not exist"
                )));
            }
        }
        Ok(())
    }

    /// User profile by name (case-insensitive, trimmed). Never a built-in.
    pub fn find_profile(&self, name: &str) -> Option<&Profile> {
        self.profiles
            .iter()
            .find(|p| p.name.trim().eq_ignore_ascii_case(name.trim()))
    }

    /// The active built-in profile, if the active profile is one.
    pub fn active_builtin(&self) -> Option<Builtin> {
        self.active_profile.as_deref().and_then(Builtin::from_name)
    }

    /// The fixed selections that apply now: the active user profile's if one is set and
    /// exists, otherwise the top-level selections. Empty when a built-in profile is
    /// active: those are resolved at toggle time (`profiles::toggle_selections`).
    pub fn effective_selections(&self) -> &[Selection] {
        if self.active_builtin().is_some() {
            return &[];
        }
        self.active_profile
            .as_deref()
            .and_then(|n| self.find_profile(n))
            .map(|p| p.selections.as_slice())
            .unwrap_or(&self.selections)
    }

    /// A profile name not used by any user profile or built-in: `base`, else
    /// `base (2)`, `base (3)`, ...
    pub fn unique_profile_name(&self, base: &str) -> String {
        let base = base.trim();
        let taken = |n: &str| Builtin::from_name(n).is_some() || self.find_profile(n).is_some();
        if !base.is_empty() && !taken(base) {
            return base.to_string();
        }
        (2..).map(|i| format!("{base} ({i})")).find(|n| !taken(n)).expect("unbounded")
    }

    /// Upgrade fixes applied when loading an existing file. `raw` is the file's parsed
    /// TOML, used to tell a missing key from a default value. Returns notes for the warning.
    fn migrate(&mut self, raw: &toml::Table) -> Vec<String> {
        let mut notes = Vec::new();
        // A user profile named like a built-in (made before built-ins existed): rename it,
        // and keep it active if it was.
        for i in 0..self.profiles.len() {
            let old = self.profiles[i].name.clone();
            if Builtin::from_name(&old).is_none() {
                continue;
            }
            let new = self.unique_profile_name(&format!("{} (custom)", old.trim()));
            if self.active_profile.as_deref().is_some_and(|a| a.trim().eq_ignore_ascii_case(old.trim())) {
                self.active_profile = Some(new.clone());
            }
            notes.push(format!("profile {old:?} renamed to {new:?} (that name is now a built-in profile)"));
            self.profiles[i].name = new;
        }
        // Existing users should not get the first-run tutorial: a file without the key
        // that already holds selections or profiles counts as seen.
        if !raw.contains_key("tutorial_seen") {
            self.tutorial_seen = !self.selections.is_empty() || !self.profiles.is_empty();
        }
        notes
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Parse and validate TOML text.
    pub fn from_toml(text: &str) -> Result<Config, ConfigError> {
        let raw: toml::Table = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let mut cfg: Config = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        cfg.migrate(&raw);
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load from the default location.
    pub fn load() -> Loaded {
        Self::load_from(&config_path())
    }

    /// Load from `path`. A missing file gives defaults silently. A corrupt or
    /// invalid file is moved to `<path>.bad` and defaults are returned with a warning.
    pub fn load_from(path: &Path) -> Loaded {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Loaded { config: Config::default(), warning: None };
            }
            Err(e) => {
                return Loaded {
                    config: Config::default(),
                    warning: Some(format!("could not read {}: {e}; using defaults", path.display())),
                };
            }
        };
        // Lenient about never-touch entries (dropped with a warning) so that a blocked
        // service never costs the user the whole config; strict about everything else.
        let parsed = toml::from_str::<toml::Table>(&text)
            .and_then(|raw| toml::from_str::<Config>(&text).map(|c| (raw, c)))
            .map_err(|e| ConfigError::Parse(e.to_string()))
            .and_then(|(raw, mut c)| {
                let notes = c.migrate(&raw);
                c.validate_inner(false).map(|_| (c, notes))
            });
        match parsed {
            Ok((mut config, mut notes)) => {
                let dropped = config.strip_blocked();
                if !dropped.is_empty() {
                    notes.push(format!("ignored selections HoldMyRAM never changes: {}", dropped.join("; ")));
                }
                let warning = (!notes.is_empty()).then(|| notes.join("; "));
                Loaded { config, warning }
            }
            Err(err) => {
                let mut bad = path.as_os_str().to_owned();
                bad.push(".bad");
                let bad = PathBuf::from(bad);
                let _ = fs::remove_file(&bad);
                let kept = match fs::rename(path, &bad) {
                    Ok(()) => format!("kept as {}", bad.display()),
                    Err(e) => format!("could not be backed up ({e})"),
                };
                Loaded {
                    config: Config::default(),
                    warning: Some(format!("{err}; file {kept}; using defaults")),
                }
            }
        }
    }

    /// Save to the default location.
    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&config_path())
    }

    /// Validate, then write atomically (temp file in the same directory + rename).
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        write_atomic(path, self.to_toml()?.as_bytes())
    }

    /// Write a copy of this config to `path` for sharing.
    pub fn export(&self, path: &Path) -> Result<(), ConfigError> {
        self.save_to(path)
    }

    /// Read and validate a config file. Nothing on disk is changed; the caller
    /// decides whether to adopt and save the result.
    pub fn import(path: &Path) -> Result<Config, ConfigError> {
        let text = fs::read_to_string(path)?;
        Config::from_toml(&text)
    }

    /// Validate `src`, and only if it is valid replace the config at `dest` with it.
    pub fn import_replace(src: &Path, dest: &Path) -> Result<Config, ConfigError> {
        let cfg = Config::import(src)?;
        cfg.save_to(dest)?;
        Ok(cfg)
    }
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir)?;
        }
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let res = fs::write(&tmp, bytes).and_then(|_| fs::rename(&tmp, path));
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Method;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn temp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "holdmyram-cfg-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn sel(name: &str, method: Method) -> Selection {
        Selection::process(name, method)
    }

    #[test]
    fn old_config_without_kind_still_loads() {
        // Written by the version before services/tasks: no `kind` anywhere.
        let dir = temp_dir();
        let p = dir.join("config.toml");
        fs::write(
            &p,
            "hotkey = \"Ctrl+Alt+R\"\ntheme = \"dark\"\ngrace_seconds = 5\nactive_profile = \"Gaming\"\n\n\
             [[selections]]\nname = \"chrome.exe\"\nmethod = \"close\"\n\n\
             [[profiles]]\nname = \"Gaming\"\n\n[[profiles.selections]]\nname = \"Discord.exe\"\nmethod = \"suspend\"\n\n\
             [[profiles.selections]]\nname = \"Spotify.exe\"\n",
        )
        .unwrap();
        let l = Config::load_from(&p);
        assert!(l.warning.is_none(), "{:?}", l.warning);
        assert_eq!(l.config.selections, vec![sel("chrome.exe", Method::Close)]);
        assert_eq!(
            l.config.profiles[0].selections,
            vec![sel("Discord.exe", Method::Suspend), sel("Spotify.exe", Method::Close)]
        );
        assert!(l.config.effective_selections().iter().all(|s| s.kind == SelectionKind::Process));
    }

    #[test]
    fn services_and_tasks_round_trip_with_kind() {
        let dir = temp_dir();
        let p = dir.join("config.toml");
        let mut c = sample();
        c.selections.push(Selection::service("SysMain"));
        c.selections.push(Selection::task(r"\Microsoft\Windows\Defrag\ScheduledDefrag"));
        c.save_to(&p).unwrap();
        let text = fs::read_to_string(&p).unwrap();
        assert!(text.contains("kind = \"service\"") && text.contains("method = \"stop\""), "{text}");
        assert!(text.contains("kind = \"task\"") && text.contains("method = \"disable\""), "{text}");
        assert_eq!(Config::load_from(&p).config, c);
        // Method may be left out for services and tasks.
        let c = Config::from_toml("[[selections]]\nname = \"SysMain\"\nkind = \"service\"\n").unwrap();
        assert_eq!(c.selections[0], Selection::service("SysMain"));
        let c = Config::from_toml("[[selections]]\nname = '\\X\\Y'\nkind = \"task\"\n").unwrap();
        assert_eq!(c.selections[0], Selection::task(r"\X\Y"));
    }

    #[test]
    fn validation_per_kind() {
        let bad = |s: Selection| {
            let mut c = Config::default();
            c.selections = vec![s.clone()];
            c.validate().map(|_| ()).map_err(|e| e.to_string()).expect_err(&format!("{s:?} accepted"))
        };
        let good = |s: Selection| {
            let mut c = Config::default();
            c.selections = vec![s.clone()];
            c.validate().unwrap_or_else(|e| panic!("{s:?}: {e}"));
        };
        good(Selection::service("SysMain"));
        good(Selection::service("CDPUserSvc_8155c"));
        good(Selection::task(r"\Microsoft\Windows\Defrag\ScheduledDefrag"));
        for m in [Method::Close, Method::Suspend, Method::Kill] {
            good(sel("a.exe", m));
        }
        // Method must match the kind.
        let e = bad(Selection { name: "SysMain".into(), method: Method::Kill, kind: SelectionKind::Service });
        assert!(e.contains("must be stop"), "{e}");
        let e = bad(Selection { name: r"\X".into(), method: Method::Stop, kind: SelectionKind::Task });
        assert!(e.contains("must be disable"), "{e}");
        bad(sel("a.exe", Method::Stop));
        bad(sel("a.exe", Method::Disable));
        // Names/paths validated with the helper's rules.
        bad(Selection::service(r"..\RpcSs"));
        bad(Selection::service(" SysMain"));
        bad(Selection::service(""));
        bad(Selection::task("NotRooted"));
        bad(Selection::task(r"\a\..\b"));
        bad(Selection::task(r"\a\"));
        // Never-touch refused at save with a clear reason.
        let e = bad(Selection::service("rpcss"));
        assert!(e.contains("Core Windows service, cannot be changed"), "{e}");
        let e = bad(Selection::service("EasyAntiCheat_EOS"));
        assert!(e.contains("Anti-cheat"), "{e}");
        for s in ["BEDaisy", "faceitclient"] {
            let e = bad(Selection::service(s));
            assert!(e.contains("Anti-cheat"), "{e}");
        }
        let e = bad(Selection::service("AudioSrv_1a2b3"));
        assert!(e.contains("Core Windows service"), "{e}");
        let e = bad(Selection::task(r"\Microsoft\Windows\Windows Defender\Windows Defender Scheduled Scan"));
        assert!(e.contains("Protected Windows task folder"), "{e}");
        // Same inside a profile.
        let mut c = sample();
        c.profiles[0].selections.push(Selection::service("WinDefend"));
        assert!(c.validate().unwrap_err().to_string().contains("Windows security service"));
        let dir = temp_dir();
        assert!(c.save_to(&dir.join("config.toml")).is_err(), "never saved");
    }

    #[test]
    fn blocked_entries_on_disk_are_dropped_not_the_whole_config() {
        let dir = temp_dir();
        let p = dir.join("config.toml");
        fs::write(
            &p,
            "theme = \"light\"\n[[selections]]\nname = \"RpcSs\"\nkind = \"service\"\n\
             [[selections]]\nname = \"SysMain\"\nkind = \"service\"\n",
        )
        .unwrap();
        let l = Config::load_from(&p);
        assert_eq!(l.config.theme, Theme::Light);
        assert_eq!(l.config.selections, vec![Selection::service("SysMain")]);
        assert!(l.warning.as_deref().unwrap_or("").contains("RpcSs"), "{:?}", l.warning);
        assert!(p.exists() && !dir.join("config.toml.bad").exists());
        // Import is strict.
        assert!(Config::import(&p).is_err());
        // A kind/method mismatch on disk is still treated as corrupt.
        fs::write(&p, "[[selections]]\nname = \"SysMain\"\nkind = \"service\"\nmethod = \"kill\"\n").unwrap();
        let l = Config::load_from(&p);
        assert!(l.warning.is_some() && dir.join("config.toml.bad").exists());
        assert!(admin_block_reason(SelectionKind::Process, "svchost.exe").is_none());
    }

    fn sample() -> Config {
        Config {
            hotkey: "Ctrl+Shift+F9".into(),
            theme: Theme::System,
            grace_seconds: 10,
            selections: vec![sel("chrome.exe", Method::Close)],
            profiles: vec![
                Profile {
                    name: "Gaming".into(),
                    selections: vec![sel("Discord.exe", Method::Suspend), sel("Spotify.exe", Method::Kill)],
                    ..Default::default()
                },
                Profile { name: "Work".into(), description: "Office hours".into(), selections: vec![] },
            ],
            active_profile: Some("Gaming".into()),
            tutorial_seen: true,
            recommended_addons: vec![AddonRef::new(SelectionKind::Process, "Discord.exe")],
            recommended_addon_groups: vec!["optin".into()],
            auto_check_updates: false,
            auto_download_updates: true,
        }
    }

    #[test]
    fn defaults() {
        let c = Config::default();
        assert_eq!(c.hotkey, "Ctrl+Alt+R");
        assert_eq!(c.theme, Theme::Dark);
        assert_eq!(c.grace_seconds, 5);
        assert!(c.selections.is_empty() && c.profiles.is_empty());
        // A brand-new config starts on the built-in Recommended profile.
        assert_eq!(c.active_profile.as_deref(), Some("Recommended"));
        assert_eq!(c.active_builtin(), Some(Builtin::Recommended));
        assert!(c.effective_selections().is_empty());
        assert!(!c.tutorial_seen);
        assert!(c.auto_check_updates && c.auto_download_updates);
        c.validate().unwrap();
    }

    #[test]
    fn update_settings_default_on_for_old_files_and_round_trip() {
        let dir = temp_dir();
        let p = dir.join("config.toml");
        fs::write(&p, "theme = \"light\"\n").unwrap();
        let c = Config::load_from(&p).config;
        assert!(c.auto_check_updates && c.auto_download_updates);
        let mut c = c;
        c.auto_check_updates = false;
        c.save_to(&p).unwrap();
        let back = Config::load_from(&p).config;
        assert!(!back.auto_check_updates && back.auto_download_updates);
    }

    #[test]
    fn existing_config_keeps_its_active_profile() {
        let dir = temp_dir();
        let p = dir.join("config.toml");
        // No active_profile key: stays None (top-level selections), not Recommended.
        fs::write(&p, "[[selections]]\nname = \"chrome.exe\"\n").unwrap();
        let l = Config::load_from(&p);
        assert_eq!(l.config.active_profile, None);
        assert_eq!(l.config.effective_selections().len(), 1);
        // A saved None stays None after a round trip.
        l.config.save_to(&p).unwrap();
        assert_eq!(Config::load_from(&p).config.active_profile, None);
        // Built-in names are valid active profiles, case-insensitively.
        fs::write(&p, "active_profile = \"recommended+\"\n").unwrap();
        let l = Config::load_from(&p);
        assert!(l.warning.is_none(), "{:?}", l.warning);
        assert_eq!(l.config.active_builtin(), Some(Builtin::RecommendedPlus));
    }

    #[test]
    fn tutorial_seen_migration() {
        let dir = temp_dir();
        let p = dir.join("config.toml");
        let load = |text: &str| {
            fs::write(&p, text).unwrap();
            Config::load_from(&p).config.tutorial_seen
        };
        // No file at all: new user.
        assert!(!Config::load_from(&dir.join("none.toml")).config.tutorial_seen);
        // Existing file without the key: seen only if it has selections or profiles.
        assert!(!load("theme = \"light\"\n"));
        assert!(load("[[selections]]\nname = \"a.exe\"\n"));
        assert!(load("[[profiles]]\nname = \"Empty\"\n"));
        // An explicit value always wins.
        assert!(!load("tutorial_seen = false\n[[selections]]\nname = \"a.exe\"\n"));
        assert!(load("tutorial_seen = true\n"));
        // Saved files carry the key, so the rule applies only once.
        let mut c = Config::load_from(&p).config;
        c.tutorial_seen = false;
        c.selections.push(sel("a.exe", Method::Close));
        c.save_to(&p).unwrap();
        assert!(fs::read_to_string(&p).unwrap().contains("tutorial_seen = false"));
        assert!(!Config::load_from(&p).config.tutorial_seen);
    }

    #[test]
    fn user_profile_named_like_a_builtin_is_renamed_on_load() {
        let dir = temp_dir();
        let p = dir.join("config.toml");
        fs::write(
            &p,
            "active_profile = \"Recommended\"\n[[profiles]]\nname = \"Recommended\"\n\
             [[profiles.selections]]\nname = \"a.exe\"\n[[profiles]]\nname = \"recommended+\"\n",
        )
        .unwrap();
        let l = Config::load_from(&p);
        let names: Vec<_> = l.config.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Recommended (custom)", "recommended+ (custom)"]);
        assert_eq!(l.config.active_profile.as_deref(), Some("Recommended (custom)"));
        assert_eq!(l.config.effective_selections().len(), 1);
        assert!(l.warning.as_deref().unwrap().contains("renamed"), "{:?}", l.warning);
        assert!(!dir.join("config.toml.bad").exists());
        // Saving such a name is refused.
        let mut c = Config::default();
        c.profiles.push(Profile { name: "RECOMMENDED".into(), ..Default::default() });
        assert!(c.validate().unwrap_err().to_string().contains("built-in"));
    }

    #[test]
    fn unique_names_avoid_builtins_and_profiles() {
        let c = sample();
        assert_eq!(c.unique_profile_name("New"), "New");
        assert_eq!(c.unique_profile_name("gaming"), "gaming (2)");
        assert_eq!(c.unique_profile_name("Recommended"), "Recommended (2)");
        let mut c2 = c.clone();
        c2.profiles.push(Profile { name: "Gaming (2)".into(), ..Default::default() });
        assert_eq!(c2.unique_profile_name("Gaming"), "Gaming (3)");
    }

    #[test]
    fn round_trip() {
        let dir = temp_dir();
        let p = dir.join("config.toml");
        let c = sample();
        c.save_to(&p).unwrap();
        let l = Config::load_from(&p);
        assert!(l.warning.is_none());
        assert_eq!(l.config, c);
        assert!(!dir.join("config.toml.tmp").exists());
        // Overwrite works too.
        let mut c2 = c.clone();
        c2.theme = Theme::Light;
        c2.save_to(&p).unwrap();
        assert_eq!(Config::load_from(&p).config.theme, Theme::Light);
    }

    #[test]
    fn missing_file_gives_defaults_without_warning() {
        let dir = temp_dir();
        let l = Config::load_from(&dir.join("nope.toml"));
        assert_eq!(l.config, Config::default());
        assert!(l.warning.is_none());
    }

    #[test]
    fn partial_file_fills_defaults() {
        let dir = temp_dir();
        let p = dir.join("config.toml");
        fs::write(&p, "theme = \"light\"\nsome_future_key = 1\n").unwrap();
        let l = Config::load_from(&p);
        assert!(l.warning.is_none());
        assert_eq!(l.config.theme, Theme::Light);
        assert_eq!(l.config.hotkey, "Ctrl+Alt+R");
        assert_eq!(l.config.grace_seconds, 5);
    }

    #[test]
    fn selection_method_defaults_to_close() {
        let c = Config::from_toml("[[selections]]\nname = \"a.exe\"\n").unwrap();
        assert_eq!(c.selections[0].method, Method::Close);
    }

    #[test]
    fn corrupt_file_is_kept_as_bad() {
        let dir = temp_dir();
        let p = dir.join("config.toml");
        fs::write(&p, "this is = = not toml [[[").unwrap();
        let l = Config::load_from(&p);
        assert_eq!(l.config, Config::default());
        assert!(l.warning.is_some());
        assert!(!p.exists());
        assert_eq!(
            fs::read_to_string(dir.join("config.toml.bad")).unwrap(),
            "this is = = not toml [[["
        );
    }

    #[test]
    fn invalid_but_parsable_file_is_treated_as_corrupt() {
        let dir = temp_dir();
        let p = dir.join("config.toml");
        fs::write(&p, "active_profile = \"ghost\"\n").unwrap();
        let l = Config::load_from(&p);
        assert_eq!(l.config, Config::default());
        assert!(l.warning.is_some());
        assert!(dir.join("config.toml.bad").exists());
    }

    #[test]
    fn effective_selections_follow_active_profile() {
        let mut c = sample();
        assert_eq!(c.effective_selections().len(), 2);
        c.active_profile = Some("work".into()); // case-insensitive
        assert!(c.effective_selections().is_empty());
        c.active_profile = None;
        assert_eq!(c.effective_selections(), &[sel("chrome.exe", Method::Close)]);
        c.active_profile = Some("missing".into());
        assert_eq!(c.effective_selections().len(), 1);
    }

    #[test]
    fn validation_rejects_bad_values() {
        let mut c = Config::default();
        c.hotkey = "  ".into();
        assert!(c.validate().is_err());

        let mut c = Config::default();
        c.grace_seconds = MAX_GRACE_SECONDS + 1;
        assert!(c.validate().is_err());

        let mut c = Config::default();
        c.selections = vec![sel("C:\\Windows\\System32\\lsass.exe", Method::Kill)];
        assert!(c.validate().is_err());

        let mut c = Config::default();
        c.selections = vec![sel("", Method::Close)];
        assert!(c.validate().is_err());

        let mut c = sample();
        c.profiles.push(Profile { name: "gaming".into(), ..Default::default() });
        assert!(c.validate().is_err());

        let mut c = sample();
        c.active_profile = Some("nope".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn save_refuses_invalid_config_and_leaves_file_alone() {
        let dir = temp_dir();
        let p = dir.join("config.toml");
        sample().save_to(&p).unwrap();
        let mut bad = sample();
        bad.hotkey.clear();
        assert!(bad.save_to(&p).is_err());
        assert_eq!(Config::load_from(&p).config, sample());
    }

    #[test]
    fn export_import_round_trip() {
        let dir = temp_dir();
        let out = dir.join("export.toml");
        sample().export(&out).unwrap();
        assert_eq!(Config::import(&out).unwrap(), sample());
    }

    #[test]
    fn import_validates_before_replacing() {
        let dir = temp_dir();
        let dest = dir.join("config.toml");
        let good = sample();
        good.save_to(&dest).unwrap();

        let bad_src = dir.join("bad.toml");
        fs::write(&bad_src, "grace_seconds = 99999\n").unwrap();
        assert!(Config::import_replace(&bad_src, &dest).is_err());
        let garbage = dir.join("garbage.toml");
        fs::write(&garbage, "[[[").unwrap();
        assert!(Config::import_replace(&garbage, &dest).is_err());
        assert!(Config::import(&dir.join("missing.toml")).is_err());
        // Destination untouched.
        assert_eq!(Config::load_from(&dest).config, good);

        let ok_src = dir.join("ok.toml");
        fs::write(&ok_src, "theme = \"light\"\n").unwrap();
        let imported = Config::import_replace(&ok_src, &dest).unwrap();
        assert_eq!(imported.theme, Theme::Light);
        assert_eq!(Config::load_from(&dest).config, imported);
    }
}
