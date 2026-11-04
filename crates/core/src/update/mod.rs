//! Self-update from GitHub Releases. No timers: the app checks on demand (once ~60 s after
//! start, and when the window asks) and this module only does the work it is asked to do.
//!
//! Flow: `check` (GET the latest-release JSON) -> `download_and_stage` (fetch
//! SHA256SUMS.txt and the zip, verify the hash, validate and extract into a staging folder)
//! -> `install::install` (only on the user's click: restore, stop the helper, swap the exes
//! by renaming, start the new exe).
//!
//! Trust model: HTTPS to fixed GitHub hosts (an allow-list checked on every redirect hop),
//! plus a SHA-256 from SHA256SUMS.txt in the same release. This protects against a corrupt
//! download and a network attacker; it does NOT protect against someone who controls the
//! GitHub account or repository (they can publish a matching zip and sums file).
//!
//! The network sits behind the `Http` trait; tests use a fake, the app uses WinHTTP.

pub mod http;
pub mod install;
pub mod worker;
pub mod zip;

use crate::admin::sys::sha256;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const REPO: &str = "holdmysocks/HoldMyRAM";
/// A check younger than this is answered from the cache unless forced.
pub const CACHE_SECS: u64 = 6 * 3600;
pub const MAX_API_BYTES: u64 = 1024 * 1024;
pub const MAX_SUMS_BYTES: u64 = 64 * 1024;
pub const MAX_ZIP_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_NOTES_CHARS: usize = 4000;
const MAX_REDIRECTS: usize = 5;
pub const SUMS_NAME: &str = "SHA256SUMS.txt";
pub const MAIN_EXE: &str = "holdmyram.exe";
pub const HELPER_EXE: &str = "holdmyram-helper.exe";

// ---------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------

/// A failure with a stable code for the UI ("offline", "not_found", "bad_checksum", ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateError {
    pub code: &'static str,
    pub msg: String,
}

impl UpdateError {
    pub fn new(code: &'static str, msg: impl Into<String>) -> UpdateError {
        UpdateError { code, msg: msg.into() }
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.msg)
    }
}

// ---------------------------------------------------------------------------------------
// Versions
// ---------------------------------------------------------------------------------------

/// Plain `MAJOR.MINOR.PATCH`. Pre-release and build suffixes are refused, so only final
/// releases are ever offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let mut it = s.split('.');
        let mut num = || -> Option<u64> {
            let p = it.next()?;
            if p.is_empty() || p.len() > 9 || !p.bytes().all(|b| b.is_ascii_digit()) || (p.len() > 1 && p.starts_with('0')) {
                return None;
            }
            p.parse().ok()
        };
        let v = Version { major: num()?, minor: num()?, patch: num()? };
        if it.next().is_some() {
            return None;
        }
        Some(v)
    }

    /// A release tag: `v1.2.3` (the `v` is optional). `v1.2.3-beta`, `v1.2.3+x` -> None.
    pub fn parse_tag(tag: &str) -> Option<Version> {
        let t = tag.trim();
        Version::parse(t.strip_prefix(['v', 'V']).unwrap_or(t))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

pub fn zip_name(v: &Version) -> String {
    format!("HoldMyRAM-{v}-win-x64.zip")
}

// ---------------------------------------------------------------------------------------
// URLs: strict parsing and the host allow-list
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub https: bool,
    /// Lowercase.
    pub host: String,
    pub port: u16,
    /// Starts with '/'; includes the query.
    pub path: String,
}

impl Url {
    pub fn parse(s: &str) -> Option<Url> {
        let (https, rest) = if let Some(r) = s.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = s.strip_prefix("http://") {
            (false, r)
        } else {
            return None;
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() || authority.contains('@') || path.chars().any(|c| c.is_control() || c == ' ' || c == '\\') {
            return None;
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().ok().filter(|&p| p != 0)?),
            None => (authority, if https { 443 } else { 80 }),
        };
        let host = host.to_ascii_lowercase();
        if host.is_empty() || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') || host.starts_with('.') || host.ends_with('.') || host.contains("..") {
            return None;
        }
        Some(Url { https, host, port, path: path.to_string() })
    }

    fn is_local(&self) -> bool {
        !self.https && self.host == "127.0.0.1"
    }

    fn is_default_https(&self) -> bool {
        self.https && self.port == 443
    }
}

/// Which URLs may be fetched. `RELEASE` is the only policy a release build can have;
/// `allow_local_http` exists for the debug-only local test hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UrlPolicy {
    pub allow_local_http: bool,
}

/// What a URL is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlRole {
    Api,
    /// The first hop of an asset download.
    Asset,
    /// Any later hop (after a redirect).
    Redirect,
    /// A page opened in the browser.
    Page,
}

impl UrlPolicy {
    pub const RELEASE: UrlPolicy = UrlPolicy { allow_local_http: false };

    pub fn check(&self, url: &str, role: UrlRole) -> Result<Url, UpdateError> {
        let bad = |why: &str| Err(UpdateError::new("bad_url", format!("refusing {url:?}: {why}")));
        let Some(u) = Url::parse(url) else { return bad("not a valid http(s) URL") };
        if self.allow_local_http && u.is_local() && role != UrlRole::Page {
            return Ok(u);
        }
        if !u.is_default_https() {
            return bad("only https on the default port is allowed");
        }
        let ok = match role {
            UrlRole::Api => u.host == "api.github.com" && u.path == format!("/repos/{REPO}/releases/latest"),
            UrlRole::Asset => u.host == "github.com" && u.path.starts_with(&format!("/{REPO}/releases/download/")),
            UrlRole::Redirect => u.host == "github.com" || u.host.ends_with(".githubusercontent.com"),
            UrlRole::Page => u.host == "github.com",
        };
        if ok {
            Ok(u)
        } else {
            bad("host or path is not on the allow-list")
        }
    }
}

pub fn default_api_url() -> String {
    format!("https://api.github.com/repos/{REPO}/releases/latest")
}

pub fn releases_page() -> String {
    format!("https://github.com/{REPO}/releases")
}

/// The API URL and URL policy to use. `env` is the debug-only override
/// (HOLDMYRAM_UPDATE_API_URL); with `debug == false` it is ignored completely.
pub fn select_api(env: Option<String>, debug: bool) -> (String, UrlPolicy) {
    match env {
        Some(u) if debug && !u.trim().is_empty() => (u.trim().to_string(), UrlPolicy { allow_local_http: true }),
        _ => (default_api_url(), UrlPolicy::RELEASE),
    }
}

/// The debug-only override. A release build has no code that reads the variable.
#[cfg(debug_assertions)]
pub fn env_override() -> Option<String> {
    std::env::var("HOLDMYRAM_UPDATE_API_URL").ok()
}
#[cfg(not(debug_assertions))]
pub fn env_override() -> Option<String> {
    None
}

/// What the running build uses.
pub fn active_api() -> (String, UrlPolicy) {
    select_api(env_override(), cfg!(debug_assertions))
}

// ---------------------------------------------------------------------------------------
// HTTP with checked redirects
// ---------------------------------------------------------------------------------------

pub use http::{Http, HttpResponse, NetError};

/// GET `url`, following up to 5 redirects; every hop is checked against `policy`.
pub fn fetch(
    net: &dyn Http,
    policy: UrlPolicy,
    url: &str,
    role: UrlRole,
    accept: &str,
    max: u64,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<HttpResponse, UpdateError> {
    let mut current = url.to_string();
    let mut role = role;
    for _ in 0..=MAX_REDIRECTS {
        let u = policy.check(&current, role)?;
        let resp = net.get(&u, accept, max, progress).map_err(|e| {
            UpdateError::new(if e.offline { "offline" } else { "network" }, e.msg)
        })?;
        if matches!(resp.status, 301 | 302 | 303 | 307 | 308) {
            let loc = resp.header("location").ok_or_else(|| UpdateError::new("network", "redirect without a location"))?;
            current = if loc.starts_with('/') {
                format!("{}://{}{}{}", if u.https { "https" } else { "http" }, u.host, if u.is_default_https() || (!u.https && u.port == 80) { String::new() } else { format!(":{}", u.port) }, loc)
            } else {
                loc.to_string()
            };
            role = UrlRole::Redirect;
            continue;
        }
        return Ok(resp);
    }
    Err(UpdateError::new("network", "too many redirects"))
}

fn http_error(resp: &HttpResponse, what: &str) -> UpdateError {
    match resp.status {
        404 => UpdateError::new("not_found", format!("{what}: not found (no published release yet, or the repository is private)")),
        429 => UpdateError::new("rate_limited", format!("{what}: GitHub rate limit reached; try again later")),
        403 if resp.header("x-ratelimit-remaining") == Some("0") => {
            UpdateError::new("rate_limited", format!("{what}: GitHub rate limit reached; try again later"))
        }
        403 => UpdateError::new("forbidden", format!("{what}: HTTP 403")),
        s => UpdateError::new("http_error", format!("{what}: HTTP {s}")),
    }
}

// ---------------------------------------------------------------------------------------
// Release JSON and SHA256SUMS
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct GhRelease {
    pub tag_name: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<GhAsset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GhAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetRef {
    pub name: String,
    pub url: String,
    pub size: u64,
}

/// Plain text, control characters removed (newlines kept), at most `MAX_NOTES_CHARS`.
pub fn clean_notes(body: &str) -> String {
    let s: String = body.replace("\r\n", "\n").chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect();
    let s = s.trim();
    if s.chars().count() > MAX_NOTES_CHARS {
        let cut: String = s.chars().take(MAX_NOTES_CHARS - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        s.to_string()
    }
}

/// The zip and sums assets for `v`, exactly by name, with allowed URLs.
pub fn pick_assets(rel: &GhRelease, v: &Version, policy: UrlPolicy) -> Result<(AssetRef, AssetRef), UpdateError> {
    let want = zip_name(v);
    let find = |name: &str| -> Result<AssetRef, UpdateError> {
        let a = rel
            .assets
            .iter()
            .find(|a| a.name == name)
            .ok_or_else(|| UpdateError::new("no_asset", format!("release has no asset named {name}")))?;
        policy
            .check(&a.browser_download_url, UrlRole::Asset)
            .map_err(|e| UpdateError::new("no_asset", format!("asset {name}: {}", e.msg)))?;
        Ok(AssetRef { name: a.name.clone(), url: a.browser_download_url.clone(), size: a.size })
    };
    let zip = find(&want)?;
    if zip.size > MAX_ZIP_BYTES {
        return Err(UpdateError::new("no_asset", format!("{want} is larger than {} MB", MAX_ZIP_BYTES / 1024 / 1024)));
    }
    Ok((zip, find(SUMS_NAME)?))
}

/// `<64 hex>  <name>` lines (optional `*` before the name, CRLF or LF). Returns the
/// lowercase hash listed for `name`.
pub fn parse_sums(text: &str, name: &str) -> Result<String, UpdateError> {
    let mut found: Option<String> = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((hash, rest)) = line.split_once(char::is_whitespace) else { continue };
        let file = rest.trim_start();
        let file = file.strip_prefix('*').unwrap_or(file);
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        if file == name {
            let h = hash.to_ascii_lowercase();
            if found.as_ref().is_some_and(|f| *f != h) {
                return Err(UpdateError::new("bad_checksum", format!("{SUMS_NAME} lists {name} twice with different hashes")));
            }
            found = Some(h);
        }
    }
    found.ok_or_else(|| UpdateError::new("bad_checksum", format!("{name} is not listed in {SUMS_NAME}; refusing to install")))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn sha256_hex(data: &[u8]) -> Result<String, UpdateError> {
    sha256(data).map(|h| hex(&h)).map_err(|e| UpdateError::new("io", e.to_string()))
}

// ---------------------------------------------------------------------------------------
// Check
// ---------------------------------------------------------------------------------------

/// Outcome of one check, persisted as the 6-hour cache.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRecord {
    /// Unix seconds.
    pub checked_at: u64,
    /// Version of the latest release, if one was read.
    pub latest: Option<String>,
    pub release_url: Option<String>,
    pub notes: Option<String>,
    pub zip: Option<AssetRef>,
    pub sums: Option<AssetRef>,
    pub error: Option<String>,
    pub error_code: Option<String>,
}

impl CheckRecord {
    pub fn latest_version(&self) -> Option<Version> {
        self.latest.as_deref().and_then(Version::parse)
    }

    /// Newer than `current`.
    pub fn available(&self, current: &Version) -> bool {
        self.latest_version().is_some_and(|l| l > *current)
    }

    /// Newer and installable (both assets known).
    pub fn installable(&self, current: &Version) -> bool {
        self.available(current) && self.zip.is_some() && self.sums.is_some()
    }

    pub fn fresh(&self, now: u64) -> bool {
        self.checked_at <= now && now - self.checked_at < CACHE_SECS
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn fail(now: u64, e: UpdateError) -> CheckRecord {
    CheckRecord { checked_at: now, error: Some(e.msg), error_code: Some(e.code.to_string()), ..Default::default() }
}

/// Ask GitHub for the latest release. Never fails: problems are recorded in the result
/// (`error`/`error_code`) and mean "no update".
pub fn check(net: &dyn Http, api_url: &str, policy: UrlPolicy, current: &Version, now: u64) -> CheckRecord {
    let resp = match fetch(net, policy, api_url, UrlRole::Api, "application/vnd.github+json", MAX_API_BYTES, &mut |_, _| {}) {
        Ok(r) => r,
        Err(e) => return fail(now, e),
    };
    if resp.status != 200 {
        return fail(now, http_error(&resp, "latest release"));
    }
    let rel: GhRelease = match serde_json::from_slice(&resp.body) {
        Ok(r) => r,
        Err(e) => return fail(now, UpdateError::new("bad_release", format!("could not read the release information: {e}"))),
    };
    record_from_release(&rel, policy, current, now)
}

pub fn record_from_release(rel: &GhRelease, policy: UrlPolicy, current: &Version, now: u64) -> CheckRecord {
    // The endpoint already excludes these; double-check anyway.
    if rel.draft || rel.prerelease {
        return CheckRecord { checked_at: now, ..Default::default() };
    }
    let Some(v) = Version::parse_tag(&rel.tag_name) else {
        return fail(now, UpdateError::new("bad_release", format!("release tag {:?} is not a plain version", rel.tag_name)));
    };
    let release_url = policy.check(&rel.html_url, UrlRole::Page).ok().map(|_| rel.html_url.clone()).or_else(|| Some(releases_page()));
    let mut rec = CheckRecord {
        checked_at: now,
        latest: Some(v.to_string()),
        release_url,
        notes: rel.body.as_deref().map(clean_notes).filter(|n| !n.is_empty()),
        ..Default::default()
    };
    if v > *current {
        match pick_assets(rel, &v, policy) {
            Ok((zip, sums)) => {
                rec.zip = Some(zip);
                rec.sums = Some(sums);
            }
            Err(e) => {
                rec.error = Some(e.msg);
                rec.error_code = Some(e.code.to_string());
            }
        }
    }
    rec
}

// ---------------------------------------------------------------------------------------
// Cache file
// ---------------------------------------------------------------------------------------

/// `%LOCALAPPDATA%\HoldMyRAM` (temp dir if unset).
pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("HoldMyRAM")
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheFile {
    #[serde(default)]
    pub last: Option<CheckRecord>,
    /// "updated from X to Y" of the most recent self-update.
    #[serde(default)]
    pub updated: Option<String>,
}

pub fn cache_path(dir: &Path) -> PathBuf {
    dir.join("update.json")
}

/// Missing or corrupt: empty (it is only a cache).
pub fn load_cache(dir: &Path) -> CacheFile {
    fs::read(cache_path(dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_cache(dir: &Path, c: &CacheFile) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(c).map_err(|e| e.to_string())?;
    crate::config::write_atomic(&cache_path(dir), &json).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------
// Download and stage
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedFile {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

/// A verified, extracted update waiting for the user's click.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Staged {
    pub version: String,
    pub dir: PathBuf,
    pub files: Vec<StagedFile>,
}

const MANIFEST: &str = "staged.json";

pub fn version_dir(updates: &Path, v: &Version) -> PathBuf {
    updates.join(v.to_string())
}

/// Fetch the sums and the zip for `rec`, verify, validate and extract into
/// `<updates>/<ver>/staged`. `progress(done, total)` is called while the zip downloads.
pub fn download_and_stage(
    net: &dyn Http,
    policy: UrlPolicy,
    rec: &CheckRecord,
    current: &Version,
    updates: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Staged, UpdateError> {
    let v = rec.latest_version().ok_or_else(|| UpdateError::new("no_update", "no release information"))?;
    if v <= *current {
        return Err(UpdateError::new("no_update", format!("{v} is not newer than {current}")));
    }
    let (zip, sums) = match (&rec.zip, &rec.sums) {
        (Some(z), Some(s)) => (z, s),
        _ => return Err(UpdateError::new("no_asset", "the release has no installable download")),
    };
    if zip.name != zip_name(&v) {
        return Err(UpdateError::new("no_asset", format!("unexpected asset name {}", zip.name)));
    }
    let resp = fetch(net, policy, &sums.url, UrlRole::Asset, "application/octet-stream", MAX_SUMS_BYTES, &mut |_, _| {})?;
    if resp.status != 200 {
        return Err(http_error(&resp, SUMS_NAME));
    }
    let sums_text = String::from_utf8(resp.body).map_err(|_| UpdateError::new("bad_checksum", format!("{SUMS_NAME} is not text")))?;
    let want = parse_sums(&sums_text, &zip.name)?;

    let resp = fetch(net, policy, &zip.url, UrlRole::Asset, "application/octet-stream", MAX_ZIP_BYTES, progress)?;
    if resp.status != 200 {
        return Err(http_error(&resp, &zip.name));
    }
    let got = sha256_hex(&resp.body)?;
    if got != want {
        return Err(UpdateError::new("bad_checksum", format!("{} does not match {SUMS_NAME} (got {got}, expected {want})", zip.name)));
    }

    let vdir = version_dir(updates, &v);
    let io = |e: std::io::Error| UpdateError::new("io", format!("{}: {e}", vdir.display()));
    fs::create_dir_all(&vdir).map_err(io)?;
    fs::write(vdir.join(&zip.name), &resp.body).map_err(io)?;
    stage_zip(&resp.body, &v, &vdir)
}

/// Validate and extract a downloaded zip into `<vdir>/staged`, check the exes and write
/// the manifest.
pub fn stage_zip(bytes: &[u8], v: &Version, vdir: &Path) -> Result<Staged, UpdateError> {
    let top = zip_name(v).trim_end_matches(".zip").to_string();
    let entries = zip::read_validated(bytes, &top)?;
    for need in [MAIN_EXE, HELPER_EXE] {
        let e = entries
            .iter()
            .find(|e| e.name == need)
            .ok_or_else(|| UpdateError::new("bad_archive", format!("the zip has no {need}")))?;
        install::check_pe_x64(&e.data).map_err(|m| UpdateError::new("bad_archive", format!("{need}: {m}")))?;
    }
    let staged_dir = vdir.join("staged");
    let io = |e: std::io::Error| UpdateError::new("io", format!("{}: {e}", staged_dir.display()));
    if staged_dir.exists() {
        fs::remove_dir_all(&staged_dir).map_err(io)?;
    }
    fs::create_dir_all(&staged_dir).map_err(io)?;
    let mut files = Vec::new();
    for e in &entries {
        fs::write(staged_dir.join(&e.name), &e.data).map_err(io)?;
        files.push(StagedFile { name: e.name.clone(), size: e.data.len() as u64, sha256: sha256_hex(&e.data)? });
    }
    let staged = Staged { version: v.to_string(), dir: staged_dir.clone(), files };
    let json = serde_json::to_vec_pretty(&staged).map_err(|e| UpdateError::new("io", e.to_string()))?;
    crate::config::write_atomic(&staged_dir.join(MANIFEST), &json).map_err(|e| UpdateError::new("io", e.to_string()))?;
    Ok(staged)
}

/// A previously staged update for `v`, re-verified file by file. None if absent or changed.
pub fn load_staged(updates: &Path, v: &Version) -> Option<Staged> {
    let dir = version_dir(updates, v).join("staged");
    let s: Staged = serde_json::from_slice(&fs::read(dir.join(MANIFEST)).ok()?).ok()?;
    if s.version != v.to_string() || s.dir != dir {
        return None;
    }
    verify_staged(&s).ok()?;
    Some(s)
}

/// Every staged file still has the recorded size and hash, and both exes are present.
pub fn verify_staged(s: &Staged) -> Result<(), UpdateError> {
    for need in [MAIN_EXE, HELPER_EXE] {
        if !s.files.iter().any(|f| f.name == need) {
            return Err(UpdateError::new("bad_archive", format!("staged update lacks {need}")));
        }
    }
    for f in &s.files {
        let data = fs::read(s.dir.join(&f.name)).map_err(|e| UpdateError::new("bad_archive", format!("{}: {e}", f.name)))?;
        if data.len() as u64 != f.size || sha256_hex(&data)? != f.sha256 {
            return Err(UpdateError::new("bad_archive", format!("staged {} changed since it was verified", f.name)));
        }
    }
    Ok(())
}

/// Remove staged/downloaded versions that are not newer than `current` (best effort).
pub fn prune_updates(updates: &Path, current: &Version) {
    let Ok(rd) = fs::read_dir(updates) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if Version::parse(&name).is_some_and(|v| v <= *current) {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

#[cfg(test)]
mod tests;
