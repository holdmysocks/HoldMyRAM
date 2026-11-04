//! User configuration: TOML on disk, atomic saves, tolerant loading, export/import.

use crate::types::Selection;
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

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
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
    pub active_profile: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            hotkey: DEFAULT_HOTKEY.to_string(),
            theme: Theme::default(),
            grace_seconds: DEFAULT_GRACE_SECONDS,
            selections: Vec::new(),
            profiles: Vec::new(),
            active_profile: None,
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

fn validate_selections(list: &[Selection], what: &str) -> Result<(), ConfigError> {
    for s in list {
        if !is_valid_exe_name(&s.name) {
            return Err(ConfigError::Invalid(format!(
                "{what}: bad process name {:?}",
                s.name
            )));
        }
    }
    Ok(())
}

impl Config {
    /// Check the config for values that would be unsafe or meaningless.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.hotkey.trim().is_empty() {
            return Err(ConfigError::Invalid("hotkey is empty".into()));
        }
        if self.grace_seconds > MAX_GRACE_SECONDS {
            return Err(ConfigError::Invalid(format!(
                "grace_seconds {} exceeds {MAX_GRACE_SECONDS}",
                self.grace_seconds
            )));
        }
        validate_selections(&self.selections, "selections")?;
        let mut seen: Vec<String> = Vec::new();
        for p in &self.profiles {
            let n = p.name.trim();
            if n.is_empty() {
                return Err(ConfigError::Invalid("profile with empty name".into()));
            }
            let lower = n.to_lowercase();
            if seen.contains(&lower) {
                return Err(ConfigError::Invalid(format!("duplicate profile {n:?}")));
            }
            seen.push(lower);
            validate_selections(&p.selections, &format!("profile {n:?}"))?;
        }
        if let Some(a) = &self.active_profile {
            if self.find_profile(a).is_none() {
                return Err(ConfigError::Invalid(format!(
                    "active profile {a:?} does not exist"
                )));
            }
        }
        Ok(())
    }

    fn find_profile(&self, name: &str) -> Option<&Profile> {
        self.profiles
            .iter()
            .find(|p| p.name.trim().eq_ignore_ascii_case(name.trim()))
    }

    /// The selections that apply now: the active profile's if one is set and
    /// exists, otherwise the top-level selections.
    pub fn effective_selections(&self) -> &[Selection] {
        self.active_profile
            .as_deref()
            .and_then(|n| self.find_profile(n))
            .map(|p| p.selections.as_slice())
            .unwrap_or(&self.selections)
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Parse and validate TOML text.
    pub fn from_toml(text: &str) -> Result<Config, ConfigError> {
        let cfg: Config = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
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
        match Config::from_toml(&text) {
            Ok(config) => Loaded { config, warning: None },
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

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
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
        Selection { name: name.into(), method }
    }

    fn sample() -> Config {
        Config {
            hotkey: "Ctrl+Shift+F9".into(),
            theme: Theme::System,
            grace_seconds: 10,
            selections: vec![sel("chrome.exe", Method::Close)],
            profiles: vec![
                Profile { name: "Gaming".into(), selections: vec![sel("Discord.exe", Method::Suspend), sel("Spotify.exe", Method::Kill)] },
                Profile { name: "Work".into(), selections: vec![] },
            ],
            active_profile: Some("Gaming".into()),
        }
    }

    #[test]
    fn defaults() {
        let c = Config::default();
        assert_eq!(c.hotkey, "Ctrl+Alt+R");
        assert_eq!(c.theme, Theme::Dark);
        assert_eq!(c.grace_seconds, 5);
        assert!(c.selections.is_empty() && c.profiles.is_empty());
        assert_eq!(c.active_profile, None);
        c.validate().unwrap();
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
        c.profiles.push(Profile { name: "gaming".into(), selections: vec![] });
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
