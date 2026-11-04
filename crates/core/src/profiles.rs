//! Profiles beyond plain selection lists:
//!
//! - Built-in, read-only, rule-based profiles "Recommended" (processes only, never needs
//!   admin) and "Recommended+" (adds services and scheduled tasks through the helper).
//!   They are resolved from `data/recommended.toml` against a fresh inventory of this PC,
//!   then filtered through the same protections the toggle uses. `resolve_with` is the one
//!   code path behind both the preview and the toggle.
//! - "Duplicate to customise": freeze the current resolution into an editable profile.
//! - Per-profile export/import as `.hmrprofile` files (TOML, see `PROFILE_FORMAT`).
//!
//! `.hmrprofile` format (TOML, version 1):
//! ```toml
//! format = "holdmyram-profile"     # required, exact
//! version = 1                      # required; newer versions are refused
//! name = "Gaming"                  # required, 1..=64 chars, no control characters
//! description = "..."              # optional, up to 1000 chars
//! source = "snapshot of ..."       # optional, informational (set on built-in exports)
//! [[selections]]
//! name = "Discord.exe"             # exe name / service short name / full task path
//! kind = "process"                 # required: process | service | task
//! method = "close"                 # optional: process close|suspend|kill, service stop, task disable
//! ```
//! Unknown keys are refused. Import is strict about structure (format, version, kinds,
//! methods, names) and lenient only about items HoldMyRAM never touches: those are dropped
//! and each drop is reported.

use crate::config::{admin_block_reason, check_shape, write_atomic, Config, Profile};
use crate::plan::{ancestors, descendants, is_protected_name, select_targets, Skipped};
use crate::process::{list_processes, ImagePaths, ProcInfo};
use crate::risk::{Kind, Risk, RiskDb};
use crate::types::{Method, Selection, SelectionKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static RECOMMENDED_TOML: &str = include_str!("../../../data/recommended.toml");

pub const RECOMMENDED: &str = "Recommended";
pub const RECOMMENDED_PLUS: &str = "Recommended+";
/// Group of user-facing optional apps (add-on only).
pub const OPTIONAL_GROUP: &str = "optional";
/// Group of other opt-in items (add-on only).
pub const OPTIN_GROUP: &str = "optin";
/// Groups that are never used unless the user turns them (or single items) on.
pub const ADDON_GROUPS: &[&str] = &[OPTIONAL_GROUP, OPTIN_GROUP];

// ---------------------------------------------------------------------------------------
// Never recommend: separate from the helper's never-touch lists (which block an action
// outright). These are items a user may still pick by hand, but the built-in profiles
// must never contain, not even as add-ons. Enforced when the rule list is parsed.
// ---------------------------------------------------------------------------------------

/// Services: update delivery (Windows and Edge), Bluetooth, remote access, OEM control.
pub const NEVER_RECOMMEND_SERVICES: &[&str] = &[
    "wuauserv",
    "UsoSvc",
    "WaaSMedicSvc",
    "BITS",
    "DoSvc",
    "edgeupdate",
    "edgeupdatem",
    "MicrosoftEdgeElevationService",
    "bthserv",
    "BthAvctpSvc",
    "Tailscale",
    "Parsec",
    "TermService",
    "SessionEnv",
    "UmRdpService",
    "ArmouryCrateControlInterface",
    "ASUSSoftwareManager",
    "ASUSSwitch",
];

/// Task path prefixes (case-insensitive): security, update, language and repair tasks.
pub const NEVER_RECOMMEND_TASK_PREFIXES: &[&str] = &[
    r"\Microsoft\Windows\AppID\SmartScreenSpecific",
    r"\Microsoft\Windows\ExploitGuard\",
    r"\Microsoft\Windows\InstallService\",
    r"\Microsoft\Windows\UNP\",
    r"\Microsoft\Windows\LanguageComponentsInstaller\",
    r"\Microsoft\Windows\Chkdsk\SyspartRepair",
    r"\Microsoft\Windows\WaaSMedic\",
    r"\Microsoft\Windows\Bluetooth\",
    r"\MicrosoftEdgeUpdateTask",
];

/// Processes: shared runtimes, updaters that carry security fixes, OEM control software,
/// remote access.
pub const NEVER_RECOMMEND_PROCESSES: &[&str] = &[
    "msedgewebview2.exe",
    "MicrosoftEdgeUpdate.exe",
    "ArmouryCrateKeyControl.exe",
    "GHelper.exe",
    "nvcontainer.exe",
    "NVDisplay.Container.exe",
    "AMDRSServ.exe",
    "parsecd.exe",
    "tailscaled.exe",
];

/// Why the built-ins may never contain this item, or None.
pub fn never_recommend(kind: SelectionKind, name: &str) -> Option<&'static str> {
    let n = name.trim();
    match kind {
        SelectionKind::Process => NEVER_RECOMMEND_PROCESSES.iter().copied().find(|x| x.eq_ignore_ascii_case(n)),
        SelectionKind::Service => {
            let hit = |s: &str| NEVER_RECOMMEND_SERVICES.iter().copied().find(|x| x.eq_ignore_ascii_case(s));
            hit(n).or_else(|| crate::admin::rules::service_template(n).and_then(hit))
        }
        SelectionKind::Task => {
            let l = n.trim_end_matches('*').to_lowercase();
            // Either the item is inside a listed prefix, or a wildcard item would cover one.
            NEVER_RECOMMEND_TASK_PREFIXES.iter().copied().find(|p| {
                let p = p.to_lowercase();
                l.starts_with(&p) || (n.ends_with('*') && p.starts_with(&l))
            })
        }
    }
}

// ---------------------------------------------------------------------------------------
// The rule list (data/recommended.toml)
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// Unverified candidate: never in the defaults.
    Low,
    Medium,
    High,
}

/// A precondition checked at resolution time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    /// No physical printer installed (virtual printers do not count).
    NoPrinter,
}

/// One rule from data/recommended.toml.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecItem {
    pub kind: SelectionKind,
    /// Exe name, service short (or per-user template) name, or full task path; a task
    /// path may end in `*` (prefix match within the last component).
    pub name: String,
    pub group: String,
    pub confidence: Confidence,
    pub why: String,
    pub side_effect: String,
    #[serde(default)]
    pub method: Option<Method>,
    #[serde(default)]
    pub display: Option<String>,
    #[serde(default)]
    pub condition: Option<Condition>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecFile {
    format_version: u32,
    #[serde(default)]
    item: Vec<RecItem>,
}

impl RecItem {
    pub fn method(&self) -> Method {
        self.method.unwrap_or(self.kind.default_method())
    }

    /// Add-on only: in an add-on group, or a low-confidence candidate.
    pub fn is_addon(&self) -> bool {
        ADDON_GROUPS.iter().any(|g| self.group.eq_ignore_ascii_case(g)) || self.confidence == Confidence::Low
    }

    /// Used by a built-in profile under `cfg`: a default item, or an add-on the user turned
    /// on (by item or by group).
    fn enabled_in(&self, cfg: &Config) -> bool {
        !self.is_addon() || self.addon_enabled(cfg)
    }

    fn addon_enabled(&self, cfg: &Config) -> bool {
        cfg.recommended_addons.iter().any(|a| a.matches(self.kind, &self.name))
            || cfg.recommended_addon_groups.iter().any(|g| g.trim().eq_ignore_ascii_case(&self.group))
    }

    fn task_prefix(&self) -> Option<&str> {
        (self.kind == SelectionKind::Task).then(|| self.name.strip_suffix('*')).flatten()
    }

    /// Whether the actual inventory name `actual` is covered by this rule.
    pub fn matches(&self, actual: &str) -> bool {
        match self.kind {
            SelectionKind::Process => actual.eq_ignore_ascii_case(&self.name),
            SelectionKind::Service => {
                actual.eq_ignore_ascii_case(&self.name)
                    || crate::admin::rules::service_template(actual).is_some_and(|t| t.eq_ignore_ascii_case(&self.name))
            }
            SelectionKind::Task => match self.task_prefix() {
                Some(prefix) => {
                    let (a, p) = (actual.to_lowercase(), prefix.to_lowercase());
                    // Prefix within the last component only: nothing after it may add a level.
                    a.starts_with(&p) && !a[p.len()..].contains('\\')
                }
                None => actual.eq_ignore_ascii_case(&self.name),
            },
        }
    }

    /// Structural checks for one rule (also used by the data test).
    fn check(&self) -> Result<(), String> {
        let who = format!("{:?}", self.name);
        for (field, v) in [("group", &self.group), ("why", &self.why), ("side_effect", &self.side_effect)] {
            if v.trim().is_empty() {
                return Err(format!("{who}: {field} is empty"));
            }
        }
        if self.group != self.group.to_lowercase() || self.group.contains(char::is_whitespace) {
            return Err(format!("{who}: group must be one lowercase word"));
        }
        if self.method.is_some() && self.kind != SelectionKind::Process {
            return Err(format!("{who}: method is only for processes"));
        }
        if self.confidence == Confidence::Low && !ADDON_GROUPS.contains(&self.group.as_str()) {
            return Err(format!("{who}: low-confidence items must be in an add-on group ({})", ADDON_GROUPS.join(", ")));
        }
        if self.condition == Some(Condition::NoPrinter) && !(self.kind == SelectionKind::Service && self.name.eq_ignore_ascii_case("Spooler")) {
            return Err(format!("{who}: condition no_printer is only meaningful for the Spooler service"));
        }
        if let Some(hit) = never_recommend(self.kind, &self.name) {
            return Err(format!("{who}: on the never-recommend list ({hit})"));
        }
        if admin_block_reason(self.kind, self.name.trim_end_matches('*')).is_some() {
            return Err(format!("{who}: on the helper's never-touch list"));
        }
        if let Some(prefix) = self.task_prefix() {
            let p = prefix.to_lowercase();
            if let Some(f) = crate::admin::rules::NEVER_TOUCH_TASK_FOLDERS.iter().find(|f| f.to_lowercase().starts_with(&p)) {
                return Err(format!("{who}: the wildcard would cover never-touch folder {f}"));
            }
        }
        if self.kind == SelectionKind::Process && is_protected_name(&self.name) {
            return Err(format!("{who}: protected process"));
        }
        // Validate as a selection; a task prefix must be valid with a name appended.
        let name = match self.task_prefix() {
            Some(p) if p.ends_with('\\') || p.is_empty() => return Err(format!("{who}: empty task prefix")),
            Some(p) => format!("{p}X"),
            None if self.name.contains('*') => return Err(format!("{who}: '*' is only allowed at the end of a task path")),
            None => self.name.clone(),
        };
        check_shape(&Selection { name, method: self.method(), kind: self.kind })
    }
}

/// Parse and check the rule list. Errors on anything malformed or duplicated.
pub fn parse_recommended(text: &str) -> Result<Vec<RecItem>, String> {
    let f: RecFile = toml::from_str(text).map_err(|e| e.to_string())?;
    if f.format_version != 1 {
        return Err(format!("unsupported format_version {}", f.format_version));
    }
    let mut seen = HashSet::new();
    for i in &f.item {
        i.check()?;
        if !seen.insert((i.kind, i.name.to_lowercase())) {
            return Err(format!("duplicate item {:?}", i.name));
        }
    }
    Ok(f.item)
}

/// The embedded rule list. If it ever failed to parse (a test guards against that) the
/// result is empty, so the built-ins act on nothing rather than on something unchecked.
pub fn recommended_items() -> &'static [RecItem] {
    static ITEMS: OnceLock<Vec<RecItem>> = OnceLock::new();
    ITEMS.get_or_init(|| parse_recommended(RECOMMENDED_TOML).unwrap_or_default())
}

// ---------------------------------------------------------------------------------------
// Built-in profiles
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum Builtin {
    Recommended,
    RecommendedPlus,
}

impl Builtin {
    pub const ALL: [Builtin; 2] = [Builtin::Recommended, Builtin::RecommendedPlus];

    pub fn name(self) -> &'static str {
        match self {
            Builtin::Recommended => RECOMMENDED,
            Builtin::RecommendedPlus => RECOMMENDED_PLUS,
        }
    }

    /// Case-insensitive, trimmed.
    pub fn from_name(name: &str) -> Option<Builtin> {
        Self::ALL.into_iter().find(|b| b.name().eq_ignore_ascii_case(name.trim()))
    }

    pub fn description(self) -> &'static str {
        match self {
            Builtin::Recommended => {
                "Background apps that are safe to close on any PC (Game Bar, Widgets, Phone Link, updaters). Never asks for admin approval."
            }
            Builtin::RecommendedPlus => {
                "Recommended, plus background services and scheduled tasks (telemetry, updaters, SysMain). Needs admin approval once per toggle."
            }
        }
    }

    /// Whether this profile may contain services and tasks.
    pub fn includes_admin(self) -> bool {
        self == Builtin::RecommendedPlus
    }

    fn allows_kind(self, k: SelectionKind) -> bool {
        self.includes_admin() || k == SelectionKind::Process
    }
}

// ---------------------------------------------------------------------------------------
// Inventory
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SvcEntry {
    pub name: String,
    pub display: String,
    pub running: bool,
    /// Hosting process, 0 if not running.
    pub pid: u32,
    /// 0 when unknown or shared with other services.
    pub ram_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEntry {
    /// Full path, `\Folder\Name`.
    pub path: String,
    pub name: String,
    pub enabled: bool,
}

/// What is on this PC right now, as far as resolution needs to know. Read once per
/// preview or toggle; never kept.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub processes: Vec<ProcInfo>,
    /// Full image path of the processes resolution cares about (protected-dir check).
    pub exe_paths: HashMap<u32, String>,
    pub services: Vec<SvcEntry>,
    pub tasks: Vec<TaskEntry>,
    /// Services and tasks were enumerated (false: only processes were read).
    pub admin_scanned: bool,
    /// Physical printers installed; None when unknown (not read, or unreadable).
    pub physical_printers: Option<usize>,
}

impl Inventory {
    /// Read the live state. `process_names` (lowercase) limits which processes get their
    /// image path looked up; `with_admin` also enumerates services and tasks (slower).
    pub fn scan(process_names: &HashSet<String>, with_admin: bool) -> Inventory {
        let processes = list_processes();
        let mut paths = ImagePaths::new();
        let exe_paths = processes
            .iter()
            .filter(|p| process_names.contains(&p.name.to_lowercase()))
            .filter_map(|p| paths.get(p.pid).map(|e| (p.pid, e)))
            .collect();
        let mut inv = Inventory { processes, exe_paths, ..Default::default() };
        if with_admin {
            for i in crate::scan::scan_all() {
                match i.kind {
                    Kind::Service => inv.services.push(SvcEntry {
                        pid: i.pids.first().copied().unwrap_or(0),
                        name: i.name,
                        display: i.display,
                        running: i.running,
                        ram_bytes: i.ram_bytes,
                    }),
                    Kind::Task => inv.tasks.push(TaskEntry {
                        path: task_path(&i.detail, &i.name),
                        name: i.name,
                        enabled: i.running,
                    }),
                    _ => {}
                }
            }
            inv.admin_scanned = true;
            inv.physical_printers = printers::installed().map(|l| printers::physical_count(&l));
        }
        inv
    }
}

/// Installed printers, read from the registry (works whether or not the spooler runs).
pub mod printers {
    use crate::startup::{read_value, wide};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_ENUMERATE_SUB_KEYS, KEY_WOW64_64KEY};

    const PRINTERS: &str = r"SYSTEM\CurrentControlSet\Control\Print\Printers";

    /// Ports used only by software printers (PDF, XPS, Fax, OneNote, print-to-file).
    const VIRTUAL_PORTS: &[&str] = &["PORTPROMPT:", "SHRFAX:", "nul:", "FILE:", "XPSPort:"];
    /// True for a printer that only writes files or sends faxes, judged by its port only
    /// (a name can be anything). Unknown or empty port means physical (conservative).
    pub fn is_virtual(_name: &str, port: &str) -> bool {
        let port = port.trim().to_ascii_lowercase();
        VIRTUAL_PORTS.iter().any(|p| p.eq_ignore_ascii_case(&port)) || port.contains("onenote")
    }

    pub fn physical_count(list: &[(String, String)]) -> usize {
        list.iter().filter(|(n, p)| !is_virtual(n, p)).count()
    }

    /// (printer name, port) for every local printer; None if the key cannot be read.
    pub fn installed() -> Option<Vec<(String, String)>> {
        let path = wide(PRINTERS);
        let mut h = HKEY::default();
        let r = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(path.as_ptr()), Some(0), KEY_ENUMERATE_SUB_KEYS | KEY_WOW64_64KEY, &mut h) };
        if r != ERROR_SUCCESS {
            return None;
        }
        let mut names = Vec::new();
        let mut ok = true;
        for i in 0..4096u32 {
            let mut buf = [0u16; 512];
            let mut len = buf.len() as u32;
            let r = unsafe { RegEnumKeyExW(h, i, Some(PWSTR(buf.as_mut_ptr())), &mut len, None, None, None, None) };
            if r == ERROR_NO_MORE_ITEMS {
                break;
            }
            if r != ERROR_SUCCESS {
                ok = false;
                break;
            }
            names.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        unsafe {
            let _ = RegCloseKey(h);
        }
        if !ok {
            return None;
        }
        let mut out = Vec::new();
        for n in names {
            // An unreadable port counts as a physical printer (conservative).
            let port = match read_value(HKEY_LOCAL_MACHINE, &format!(r"{PRINTERS}\{n}"), "Port") {
                Ok(Some(d)) => {
                    let u: Vec<u16> = d.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                    let end = u.iter().position(|&c| c == 0).unwrap_or(u.len());
                    String::from_utf16_lossy(&u[..end])
                }
                _ => String::new(),
            };
            out.push((n, port));
        }
        Some(out)
    }
}

/// Full task path from a scan row's folder (`detail`) and name.
pub fn task_path(folder: &str, name: &str) -> String {
    if folder.ends_with('\\') {
        format!("{folder}{name}")
    } else {
        format!("{folder}\\{name}")
    }
}

// ---------------------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------------------

/// Who is asking: the foreground app at the moment of the request, and our own PID.
#[derive(Debug, Clone, Copy)]
pub struct Ctx {
    pub foreground: Option<u32>,
    pub self_pid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Not running / not installed on this PC.
    NotPresent,
    /// Service present but already stopped.
    NotRunning,
    /// Task present but already disabled.
    Disabled,
    /// In (or hosting) the foreground app's process tree.
    Foreground,
    /// HoldMyRAM itself.
    SelfProcess,
    /// Protected process name or runs from a protected directory.
    Protected,
    /// On the never-touch services / task-folder list.
    NeverTouch,
    /// The risk database (including the user's overrides) rates it Dangerous.
    Dangerous,
    /// Malformed name.
    Invalid,
    /// Services/tasks were not enumerated for this resolution.
    NotScanned,
    /// The item's condition does not hold (e.g. a printer is installed), or is unknown.
    ConditionNotMet,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedItem {
    pub kind: SelectionKind,
    /// The actual name acted on (e.g. the per-user instance `CDPUserSvc_8155c`).
    pub name: String,
    pub display: String,
    pub method: Method,
    /// Working set of the processes involved, when known.
    pub ram_bytes: Option<u64>,
    pub pids: Vec<u32>,
    pub group: String,
    pub why: String,
    pub side_effect: String,
    /// From data/recommended.toml; None for items of user profiles.
    pub confidence: Option<Confidence>,
    /// Low confidence: an unverified candidate (only ever here because the user added it).
    pub candidate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedItem {
    pub kind: SelectionKind,
    pub name: String,
    pub display: String,
    pub reason: SkipReason,
    pub detail: String,
}

/// An optional item the UI can offer as an add-on to the built-ins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AddonView {
    pub kind: SelectionKind,
    /// Pass back in `Config::recommended_addons` as is.
    pub name: String,
    pub display: String,
    /// "optional" (user-facing apps) or "optin" (everything else); low-confidence items in
    /// other groups are add-ons too.
    pub group: String,
    pub confidence: Confidence,
    /// Low confidence: show as "candidate", not "verified".
    pub candidate: bool,
    pub why: String,
    pub side_effect: String,
    /// On through `recommended_addons` (this item) or `recommended_addon_groups` (its group).
    pub enabled: bool,
    /// Running / present (services running, tasks enabled) on this PC now.
    pub present: bool,
    pub ram_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resolution {
    pub profile: String,
    pub builtin: bool,
    /// Some item is a service or task (the toggle will ask for admin approval).
    pub needs_admin: bool,
    /// What the toggle would act on right now.
    pub items: Vec<ResolvedItem>,
    /// Items left out, except plain "not present" ones (those are only counted).
    pub skipped: Vec<SkippedItem>,
    /// Count of left-out items per reason, including not_present.
    pub skipped_counts: BTreeMap<SkipReason, usize>,
    /// Sum of the known `ram_bytes` of `items`.
    pub total_ram_bytes: u64,
    /// Built-ins only: the optional add-ons and whether each is on.
    pub addons: Vec<AddonView>,
}

impl Resolution {
    /// The selections the toggle applies (same order as `items`).
    pub fn selections(&self) -> Vec<Selection> {
        self.items.iter().map(|i| Selection { name: i.name.clone(), method: i.method, kind: i.kind }).collect()
    }
}

/// Something a profile asks for, before it is matched against the inventory.
#[derive(Debug, Clone)]
struct Candidate {
    rule: RecItem,
    /// From a built-in (data/recommended.toml). Built-ins honour the risk database
    /// (Dangerous is never used) and conditions; user selections are taken as chosen.
    builtin: bool,
}

/// Which profile to resolve.
#[derive(Debug, Clone, Copy)]
pub enum ProfileRef<'a> {
    Builtin(Builtin),
    User(&'a Profile),
    /// No profile active: the top-level selections.
    TopLevel,
}

impl ProfileRef<'_> {
    pub fn label(&self) -> String {
        match self {
            ProfileRef::Builtin(b) => b.name().to_string(),
            ProfileRef::User(p) => p.name.clone(),
            ProfileRef::TopLevel => String::new(),
        }
    }
}

/// A profile by name: built-in first, then user profiles. An empty name means the
/// top-level selections.
pub fn lookup<'a>(cfg: &'a Config, name: &str) -> Option<ProfileRef<'a>> {
    if name.trim().is_empty() {
        return Some(ProfileRef::TopLevel);
    }
    if let Some(b) = Builtin::from_name(name) {
        return Some(ProfileRef::Builtin(b));
    }
    cfg.find_profile(name).map(ProfileRef::User)
}

/// The active profile (falls back to the top-level selections like `effective_selections`).
pub fn active(cfg: &Config) -> ProfileRef<'_> {
    cfg.active_profile.as_deref().and_then(|n| lookup(cfg, n)).unwrap_or(ProfileRef::TopLevel)
}

fn kind_to_risk(k: SelectionKind) -> Kind {
    match k {
        SelectionKind::Process => Kind::Process,
        SelectionKind::Service => Kind::Service,
        SelectionKind::Task => Kind::Task,
    }
}

fn candidates(cfg: &Config, p: ProfileRef<'_>, db: &RiskDb) -> Vec<Candidate> {
    match p {
        ProfileRef::Builtin(b) => recommended_items()
            .iter()
            .filter(|r| b.allows_kind(r.kind) && r.enabled_in(cfg))
            .map(|r| Candidate { rule: r.clone(), builtin: true })
            .collect(),
        ProfileRef::User(prof) => user_candidates(&prof.selections, db),
        ProfileRef::TopLevel => user_candidates(&cfg.selections, db),
    }
}

fn user_candidates(sels: &[Selection], db: &RiskDb) -> Vec<Candidate> {
    sels.iter()
        .map(|s| Candidate {
            rule: RecItem {
                kind: s.kind,
                name: s.name.clone(),
                group: "custom".into(),
                confidence: Confidence::Medium,
                why: db.lookup_for(&s.name, kind_to_risk(s.kind)).summary,
                side_effect: String::new(),
                method: Some(s.method),
                display: None,
                condition: None,
            },
            builtin: false,
        })
        .collect()
}

fn risk_of(db: &RiskDb, kind: SelectionKind, name: &str) -> Risk {
    let r = db.lookup_for(name, kind_to_risk(kind)).risk;
    if r == Risk::Unrated && kind == SelectionKind::Task {
        let leaf = name.rsplit('\\').next().unwrap_or(name);
        return db.risk(leaf, Kind::Task);
    }
    r
}

/// Match `p`'s candidates against `inv` and apply every protection. Pure: reads only its
/// arguments, so it is unit-testable with a synthetic inventory.
pub fn resolve(cfg: &Config, p: ProfileRef<'_>, inv: &Inventory, ctx: Ctx, db: &RiskDb) -> Resolution {
    let fg = crate::toggle::effective_foreground(&inv.processes, ctx.foreground, ctx.self_pid);
    let mut guard: HashSet<u32> = HashSet::new();
    if let Some(fg) = fg {
        guard.extend(descendants(&inv.processes, fg));
        guard.extend(ancestors(&inv.processes, fg));
    }
    guard.remove(&0);

    let mut out = Resolution {
        profile: p.label(),
        builtin: matches!(p, ProfileRef::Builtin(_)),
        needs_admin: false,
        items: Vec::new(),
        skipped: Vec::new(),
        skipped_counts: BTreeMap::new(),
        total_ram_bytes: 0,
        addons: Vec::new(),
    };
    let mut taken: HashSet<(SelectionKind, String)> = HashSet::new();
    let skip = |out: &mut Resolution, kind, name: &str, display: &str, reason, detail: &str| {
        *out.skipped_counts.entry(reason).or_insert(0) += 1;
        if reason != SkipReason::NotPresent {
            out.skipped.push(SkippedItem {
                kind,
                name: name.to_string(),
                display: display.to_string(),
                reason,
                detail: detail.to_string(),
            });
        }
    };

    for c in candidates(cfg, p, db) {
        let r = &c.rule;
        let disp = |actual: &str, fallback: &str| r.display.clone().unwrap_or_else(|| if fallback.is_empty() { actual.to_string() } else { fallback.to_string() });
        let mut item = |out: &mut Resolution, name: &str, display: String, ram: Option<u64>, pids: Vec<u32>| {
            if !taken.insert((r.kind, name.to_lowercase())) {
                return;
            }
            out.items.push(ResolvedItem {
                kind: r.kind,
                name: name.to_string(),
                display,
                method: r.method(),
                ram_bytes: ram,
                pids,
                group: r.group.clone(),
                why: r.why.clone(),
                side_effect: r.side_effect.clone(),
                confidence: c.builtin.then_some(r.confidence),
                candidate: c.builtin && r.confidence == Confidence::Low,
            });
        };
        if c.builtin && r.task_prefix().is_none() && risk_of(db, r.kind, &r.name) == Risk::Dangerous {
            skip(&mut out, r.kind, &r.name, &disp(&r.name, ""), SkipReason::Dangerous, "rated Dangerous in the risk database");
            continue;
        }
        if c.builtin {
            if let Some(cond) = r.condition {
                let met = match cond {
                    Condition::NoPrinter => match inv.physical_printers {
                        Some(0) => Ok(()),
                        Some(n) => Err(format!("{n} printer(s) installed; only stopped on PCs without a printer")),
                        None => Err("could not tell whether a printer is installed; left alone".to_string()),
                    },
                };
                if let Err(why) = met {
                    skip(&mut out, r.kind, &r.name, &disp(&r.name, ""), SkipReason::ConditionNotMet, &why);
                    continue;
                }
            }
        }
        match r.kind {
            SelectionKind::Process => {
                if check_shape(&Selection::process(r.name.clone(), r.method())).is_err() {
                    skip(&mut out, r.kind, &r.name, &r.name, SkipReason::Invalid, "not a plain executable name");
                    continue;
                }
                if !inv.processes.iter().any(|q| q.name.eq_ignore_ascii_case(&r.name)) {
                    skip(&mut out, r.kind, &r.name, &disp(&r.name, ""), SkipReason::NotPresent, "not running");
                    continue;
                }
                let (targets, skipped) = select_targets(&inv.processes, std::slice::from_ref(&r.name), fg, ctx.self_pid);
                if targets.is_empty() {
                    let (reason, why) = match skipped.first().map(|(_, w)| w) {
                        Some(Skipped::Foreground) => (SkipReason::Foreground, "foreground app (or part of its process tree); left running"),
                        Some(Skipped::SelfProcess) => (SkipReason::SelfProcess, "HoldMyRAM itself"),
                        _ => (SkipReason::Protected, "protected system process"),
                    };
                    skip(&mut out, r.kind, &r.name, &disp(&r.name, ""), reason, why);
                    continue;
                }
                // Same rule as apply: one instance in a protected directory leaves the whole app alone.
                if let Some(exe) = targets.iter().find_map(|t| inv.exe_paths.get(&t.pid).filter(|e| crate::actions::is_protected_path(e))) {
                    skip(&mut out, r.kind, &r.name, &disp(&r.name, ""), SkipReason::Protected, &format!("runs from a protected directory: {exe}"));
                    continue;
                }
                let actual = targets[0].name.clone();
                let ram: u64 = targets.iter().map(|t| t.working_set_bytes).sum();
                let pids = targets.iter().map(|t| t.pid).collect();
                item(&mut out, &actual, disp(&actual, ""), Some(ram), pids);
            }
            SelectionKind::Service => {
                if !inv.admin_scanned {
                    skip(&mut out, r.kind, &r.name, &disp(&r.name, ""), SkipReason::NotScanned, "services were not read");
                    continue;
                }
                let matches: Vec<&SvcEntry> = inv.services.iter().filter(|s| r.matches(&s.name)).collect();
                if matches.is_empty() {
                    skip(&mut out, r.kind, &r.name, &disp(&r.name, ""), SkipReason::NotPresent, "not installed");
                    continue;
                }
                let mut pending: Vec<(&SvcEntry, SkipReason, String)> = Vec::new();
                let mut used = false;
                for s in matches {
                    let reason = if let Some(why) = admin_block_reason(SelectionKind::Service, &s.name) {
                        Some((SkipReason::NeverTouch, why))
                    } else if crate::admin::rules::validate_service_name(&s.name).is_err() {
                        Some((SkipReason::Invalid, "malformed service name".into()))
                    } else if c.builtin && risk_of(db, r.kind, &s.name) == Risk::Dangerous {
                        Some((SkipReason::Dangerous, "rated Dangerous in the risk database".into()))
                    } else if !s.running {
                        Some((SkipReason::NotRunning, "already stopped".into()))
                    } else if s.pid != 0 && guard.contains(&s.pid) {
                        Some((SkipReason::Foreground, "runs in (or hosts) the foreground app's process tree; left running".into()))
                    } else if s.pid != 0 && s.pid == ctx.self_pid {
                        Some((SkipReason::SelfProcess, "runs in HoldMyRAM's own process".into()))
                    } else {
                        None
                    };
                    match reason {
                        Some((reason, why)) => pending.push((s, reason, why)),
                        None => {
                            used = true;
                            let ram = (s.ram_bytes > 0).then_some(s.ram_bytes);
                            let pids = if s.pid != 0 { vec![s.pid] } else { Vec::new() };
                            item(&mut out, &s.name, disp(&s.name, &s.display), ram, pids);
                        }
                    }
                }
                for (s, reason, why) in pending {
                    // A stopped per-user template next to its running instance is noise.
                    if used && reason == SkipReason::NotRunning {
                        continue;
                    }
                    skip(&mut out, r.kind, &s.name, &disp(&s.name, &s.display), reason, &why);
                }
            }
            SelectionKind::Task => {
                if !inv.admin_scanned {
                    skip(&mut out, r.kind, &r.name, &disp(&r.name, ""), SkipReason::NotScanned, "tasks were not read");
                    continue;
                }
                let matches: Vec<&TaskEntry> = inv.tasks.iter().filter(|t| r.matches(&t.path)).collect();
                if matches.is_empty() {
                    skip(&mut out, r.kind, &r.name, &disp(&r.name, ""), SkipReason::NotPresent, "not present");
                    continue;
                }
                for t in matches {
                    let shown = disp(&t.path, &t.name);
                    if let Some(why) = admin_block_reason(SelectionKind::Task, &t.path) {
                        skip(&mut out, r.kind, &t.path, &shown, SkipReason::NeverTouch, &why);
                    } else if crate::admin::rules::validate_task_path(&t.path).is_err() {
                        skip(&mut out, r.kind, &t.path, &shown, SkipReason::Invalid, "malformed task path");
                    } else if c.builtin && risk_of(db, r.kind, &t.path) == Risk::Dangerous {
                        skip(&mut out, r.kind, &t.path, &shown, SkipReason::Dangerous, "rated Dangerous in the risk database");
                    } else if !t.enabled {
                        skip(&mut out, r.kind, &t.path, &shown, SkipReason::Disabled, "already disabled");
                    } else {
                        item(&mut out, &t.path, shown, None, Vec::new());
                    }
                }
            }
        }
    }

    out.needs_admin = out.items.iter().any(|i| i.kind.is_admin());
    out.total_ram_bytes = out.items.iter().filter_map(|i| i.ram_bytes).sum();
    if let ProfileRef::Builtin(b) = p {
        out.addons = addon_views(cfg, b, inv);
    }
    out
}

fn addon_views(cfg: &Config, b: Builtin, inv: &Inventory) -> Vec<AddonView> {
    recommended_items()
        .iter()
        .filter(|r| r.is_addon() && b.allows_kind(r.kind))
        .map(|r| {
            let (present, ram) = match r.kind {
                SelectionKind::Process => {
                    let ps: Vec<&ProcInfo> = inv.processes.iter().filter(|p| p.name.eq_ignore_ascii_case(&r.name)).collect();
                    (!ps.is_empty(), (!ps.is_empty()).then(|| ps.iter().map(|p| p.working_set_bytes).sum()))
                }
                SelectionKind::Service => (inv.services.iter().any(|s| r.matches(&s.name) && s.running), None),
                SelectionKind::Task => (inv.tasks.iter().any(|t| r.matches(&t.path) && t.enabled), None),
            };
            AddonView {
                kind: r.kind,
                name: r.name.clone(),
                display: r.display.clone().unwrap_or_else(|| r.name.clone()),
                group: r.group.clone(),
                confidence: r.confidence,
                candidate: r.confidence == Confidence::Low,
                why: r.why.clone(),
                side_effect: r.side_effect.clone(),
                enabled: r.addon_enabled(cfg),
                present,
                ram_bytes: ram,
            }
        })
        .collect()
}

/// What `p` needs read from the system: lowercase process names (for image paths) and
/// whether services/tasks must be enumerated.
fn needs(cfg: &Config, p: ProfileRef<'_>, db: &RiskDb) -> (HashSet<String>, bool) {
    let mut cands = candidates(cfg, p, db);
    if let ProfileRef::Builtin(b) = p {
        // Add-on presence is shown in the preview, so read those too.
        cands.extend(
            recommended_items()
                .iter()
                .filter(|r| r.is_addon() && b.allows_kind(r.kind))
                .map(|r| Candidate { rule: r.clone(), builtin: true }),
        );
    }
    let names = cands.iter().filter(|c| c.rule.kind == SelectionKind::Process).map(|c| c.rule.name.to_lowercase()).collect();
    (names, cands.iter().any(|c| c.rule.kind.is_admin()))
}

/// THE resolution path shared by the preview and the toggle: work out what to read,
/// read it through `scan`, resolve.
pub fn resolve_with(
    cfg: &Config,
    p: ProfileRef<'_>,
    ctx: Ctx,
    db: &RiskDb,
    scan: impl FnOnce(&HashSet<String>, bool) -> Inventory,
) -> Resolution {
    let (names, admin) = needs(cfg, p, db);
    let inv = scan(&names, admin);
    resolve(cfg, p, &inv, ctx, db)
}

/// Preview of profile `name` against the live system. Changes nothing.
pub fn preview(cfg: &Config, name: &str, ctx: Ctx, db: &RiskDb) -> Result<Resolution, String> {
    let p = lookup(cfg, name).ok_or_else(|| format!("no profile named {name:?}"))?;
    Ok(resolve_with(cfg, p, ctx, db, Inventory::scan))
}

/// What a toggle-on applies. A built-in active profile is resolved now (same path as the
/// preview) and the resolution is returned for logging; otherwise the fixed selections are
/// returned unchanged, exactly as before built-ins existed.
pub fn toggle_selections(
    cfg: &Config,
    ctx: Ctx,
    db: &RiskDb,
    scan: impl FnOnce(&HashSet<String>, bool) -> Inventory,
) -> (Vec<Selection>, Option<Resolution>) {
    match active(cfg) {
        p @ ProfileRef::Builtin(_) => {
            let r = resolve_with(cfg, p, ctx, db, scan);
            (r.selections(), Some(r))
        }
        _ => (cfg.effective_selections().to_vec(), None),
    }
}

// ---------------------------------------------------------------------------------------
// Editing rules: built-ins are immutable; duplicate freezes
// ---------------------------------------------------------------------------------------

/// Refuse any change to a built-in profile.
pub fn ensure_editable(name: &str) -> Result<(), String> {
    match Builtin::from_name(name) {
        Some(b) => Err(format!("{} is a built-in profile and cannot be edited or deleted; use Duplicate to customise it", b.name())),
        None => Ok(()),
    }
}

/// A normal, editable profile holding a frozen copy of `r`'s current items.
pub fn freeze(r: &Resolution, name: &str, description: &str) -> Profile {
    Profile { name: name.to_string(), description: description.to_string(), selections: r.selections() }
}

/// "Duplicate to customise": add a copy of profile `name` to `cfg` under a unique name
/// (`new_name` or "<name> (copy)"). A built-in needs `resolution` (its current resolution,
/// from `preview`), which is frozen. Returns the new profile's name. Nothing is saved.
pub fn duplicate(cfg: &mut Config, name: &str, new_name: Option<&str>, resolution: Option<&Resolution>) -> Result<String, String> {
    let wanted = new_name.map(str::trim).filter(|n| !n.is_empty());
    let (base, profile) = match lookup(cfg, name) {
        Some(ProfileRef::Builtin(b)) => {
            let r = resolution.ok_or("internal: a built-in profile needs its current resolution to duplicate")?;
            let desc = format!("Copy of {} as it resolved on this PC; edit freely.", b.name());
            (wanted.unwrap_or(&format!("{} (copy)", b.name())).to_string(), freeze(r, "", &desc))
        }
        Some(ProfileRef::User(p)) => (wanted.unwrap_or(&format!("{} (copy)", p.name.trim())).to_string(), p.clone()),
        Some(ProfileRef::TopLevel) | None => return Err(format!("no profile named {name:?}")),
    };
    check_profile_name(&base)?;
    let new = cfg.unique_profile_name(&base);
    cfg.profiles.push(Profile { name: new.clone(), ..profile });
    Ok(new)
}

// ---------------------------------------------------------------------------------------
// .hmrprofile export / import
// ---------------------------------------------------------------------------------------

pub const PROFILE_FORMAT: &str = "holdmyram-profile";
pub const PROFILE_VERSION: u32 = 1;
pub const PROFILE_EXTENSION: &str = "hmrprofile";
pub const MAX_PROFILE_FILE_BYTES: u64 = 256 * 1024;
pub const MAX_PROFILE_ITEMS: usize = 2000;
pub const MAX_PROFILE_NAME: usize = 64;
pub const MAX_PROFILE_DESCRIPTION: usize = 1000;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    format: String,
    version: u32,
    name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(default)]
    selections: Vec<FileSel>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSel {
    name: String,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    method: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Dropped {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportResult {
    /// Final name (suffixed " (2)" etc. on a collision).
    pub profile_name: String,
    pub imported_count: usize,
    pub dropped: Vec<Dropped>,
    pub warnings: Vec<String>,
    /// The profile as imported (already added to the config by the app command).
    pub profile: Profile,
}

fn check_profile_name(n: &str) -> Result<(), String> {
    if n.trim().is_empty() {
        return Err("profile name is empty".into());
    }
    if n.chars().count() > MAX_PROFILE_NAME {
        return Err(format!("profile name is longer than {MAX_PROFILE_NAME} characters"));
    }
    if n.chars().any(char::is_control) {
        return Err("profile name contains a control character".into());
    }
    Ok(())
}

fn kind_str(k: SelectionKind) -> &'static str {
    match k {
        SelectionKind::Process => "process",
        SelectionKind::Service => "service",
        SelectionKind::Task => "task",
    }
}

fn method_str(m: Method) -> &'static str {
    match m {
        Method::Close => "close",
        Method::Suspend => "suspend",
        Method::Kill => "kill",
        Method::Stop => "stop",
        Method::Disable => "disable",
    }
}

/// `.hmrprofile` text for a profile.
pub fn export_text(p: &Profile, source: Option<String>) -> Result<String, String> {
    check_profile_name(&p.name)?;
    let f = ProfileFile {
        format: PROFILE_FORMAT.into(),
        version: PROFILE_VERSION,
        name: p.name.trim().to_string(),
        description: p.description.clone(),
        source,
        selections: p
            .selections
            .iter()
            .map(|s| FileSel { name: s.name.clone(), kind: kind_str(s.kind).into(), method: Some(method_str(s.method).into()) })
            .collect(),
    };
    toml::to_string_pretty(&f).map_err(|e| e.to_string())
}

/// Path with the `.hmrprofile` extension added when missing.
pub fn with_profile_extension(path: &Path) -> PathBuf {
    let ok = path.extension().is_some_and(|e| e.eq_ignore_ascii_case(PROFILE_EXTENSION));
    if ok {
        path.to_path_buf()
    } else {
        let mut s = path.as_os_str().to_owned();
        s.push(".");
        s.push(PROFILE_EXTENSION);
        PathBuf::from(s)
    }
}

/// Write profile `name` of `cfg` to `path` (atomically; `.hmrprofile` appended if missing).
/// A built-in needs `resolution`: the file is a frozen snapshot of it. Returns the path.
pub fn export_profile(cfg: &Config, name: &str, path: &Path, resolution: Option<&Resolution>) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err("no file chosen".into());
    }
    let (profile, source) = match lookup(cfg, name) {
        Some(ProfileRef::Builtin(b)) => {
            let r = resolution.ok_or("internal: a built-in profile needs its current resolution to export")?;
            let desc = format!("Snapshot of the built-in {} profile as it resolved on the exporting PC.", b.name());
            (freeze(r, &format!("{} snapshot", b.name()), &desc), Some(format!("snapshot of built-in {}", b.name())))
        }
        Some(ProfileRef::User(p)) => (p.clone(), None),
        _ => return Err(format!("no profile named {name:?}")),
    };
    let text = export_text(&profile, source)?;
    let out = with_profile_extension(path);
    write_atomic(&out, text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Parse and validate `.hmrprofile` text against `cfg` (for name collisions). Nothing is
/// changed; the caller adds `result.profile` to the config.
pub fn import_text(text: &str, cfg: &Config) -> Result<ImportResult, String> {
    if text.len() as u64 > MAX_PROFILE_FILE_BYTES {
        return Err(format!("file is larger than {} KB", MAX_PROFILE_FILE_BYTES / 1024));
    }
    // Header first, so a newer version gets a clear message rather than a schema error.
    let raw: toml::Table = toml::from_str(text).map_err(|e| format!("not a HoldMyRAM profile file: {e}"))?;
    match raw.get("format").and_then(|v| v.as_str()) {
        Some(PROFILE_FORMAT) => {}
        Some(other) => return Err(format!("not a HoldMyRAM profile file (format {other:?})")),
        None => return Err("not a HoldMyRAM profile file (no format = \"holdmyram-profile\")".into()),
    }
    match raw.get("version").and_then(|v| v.as_integer()) {
        Some(v) if v == i64::from(PROFILE_VERSION) => {}
        Some(v) if v > i64::from(PROFILE_VERSION) => {
            return Err(format!("this profile was made by a newer HoldMyRAM (version {v}); update HoldMyRAM to import it"))
        }
        Some(v) => return Err(format!("unsupported profile version {v}")),
        None => return Err("profile file has no version".into()),
    }
    let f: ProfileFile = toml::from_str(text).map_err(|e| format!("invalid profile file: {e}"))?;
    check_profile_name(&f.name)?;
    if f.description.chars().count() > MAX_PROFILE_DESCRIPTION {
        return Err(format!("description is longer than {MAX_PROFILE_DESCRIPTION} characters"));
    }
    if f.description.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
        return Err("description contains a control character".into());
    }
    if f.selections.len() > MAX_PROFILE_ITEMS {
        return Err(format!("too many selections (more than {MAX_PROFILE_ITEMS})"));
    }

    let mut warnings = Vec::new();
    let mut dropped = Vec::new();
    let mut sels: Vec<Selection> = Vec::new();
    let mut seen: HashSet<(SelectionKind, String)> = HashSet::new();
    for (i, s) in f.selections.iter().enumerate() {
        let at = format!("selection {} ({:?})", i + 1, s.name);
        let kind = match s.kind.as_str() {
            "process" => SelectionKind::Process,
            "service" => SelectionKind::Service,
            "task" => SelectionKind::Task,
            other => return Err(format!("{at}: unknown kind {other:?} (expected process, service or task)")),
        };
        let method = match s.method.as_deref() {
            None => kind.default_method(),
            Some("close") => Method::Close,
            Some("suspend") => Method::Suspend,
            Some("kill") => Method::Kill,
            Some("stop") => Method::Stop,
            Some("disable") => Method::Disable,
            Some(other) => return Err(format!("{at}: unknown method {other:?}")),
        };
        let sel = Selection { name: s.name.clone(), method, kind };
        check_shape(&sel).map_err(|e| format!("{at}: {e}"))?;
        if let Some(why) = admin_block_reason(kind, &s.name) {
            dropped.push(Dropped { name: s.name.clone(), reason: why });
            continue;
        }
        if kind == SelectionKind::Process && is_protected_name(&s.name) {
            dropped.push(Dropped { name: s.name.clone(), reason: "Protected system process, never closed".into() });
            continue;
        }
        if !seen.insert((kind, s.name.to_lowercase())) {
            warnings.push(format!("{:?} was listed more than once; kept the first", s.name));
            continue;
        }
        sels.push(sel);
    }

    let wanted = f.name.trim();
    let name = cfg.unique_profile_name(wanted);
    if name != wanted {
        warnings.push(format!("a profile named {wanted:?} already exists; imported as {name:?}"));
    }
    if sels.is_empty() {
        warnings.push("the profile has no usable selections".into());
    }
    if let Some(src) = &f.source {
        warnings.push(format!("this file is a {src}; it will not update itself"));
    }
    let profile = Profile { name: name.clone(), description: f.description, selections: sels };
    Ok(ImportResult { profile_name: name, imported_count: profile.selections.len(), dropped, warnings, profile })
}

/// Read and validate a `.hmrprofile` file (size-limited before reading).
pub fn import_file(path: &Path, cfg: &Config) -> Result<ImportResult, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    if meta.len() > MAX_PROFILE_FILE_BYTES {
        return Err(format!("file is larger than {} KB", MAX_PROFILE_FILE_BYTES / 1024));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let text = String::from_utf8(bytes).map_err(|_| "file is not UTF-8 text".to_string())?;
    import_text(text.trim_start_matches('\u{feff}'), cfg)
}

/// Add an imported profile to `cfg` (never replaces an existing one).
pub fn adopt(cfg: &mut Config, r: &ImportResult) -> Result<(), String> {
    if lookup(cfg, &r.profile.name).is_some() {
        return Err(format!("a profile named {:?} already exists", r.profile.name));
    }
    cfg.profiles.push(r.profile.clone());
    Ok(())
}

#[cfg(test)]
mod tests;
