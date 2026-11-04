use super::install::{self, InstallHost, RestoreOutcome};
use super::zip as z;
use super::*;
use crate::journal::tests::temp_dir;
use std::collections::HashMap;
use std::sync::Mutex;

// ------------------------------------------------------------------- helpers

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

/// A minimal valid PE32+ x64 image (headers only) with `tag` appended.
fn fake_pe(tag: &str) -> Vec<u8> {
    let mut b = vec![0u8; 0x200];
    b[..2].copy_from_slice(b"MZ");
    b[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
    b[0x80..0x84].copy_from_slice(b"PE\0\0");
    b[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
    b[0x98..0x9a].copy_from_slice(&0x20Bu16.to_le_bytes());
    b.extend_from_slice(tag.as_bytes());
    b
}

struct ZEntry<'a> {
    name: &'a str,
    data: Vec<u8>,
    deflate: bool,
    /// Override the recorded uncompressed size / flags / crc to build bad archives.
    usize_override: Option<u32>,
    flags: u16,
    crc_override: Option<u32>,
}

fn ze<'a>(name: &'a str, data: &[u8]) -> ZEntry<'a> {
    ZEntry { name, data: data.to_vec(), deflate: true, usize_override: None, flags: 0, crc_override: None }
}

fn make_zip(entries: &[ZEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cd = Vec::new();
    for e in entries {
        let crc = e.crc_override.unwrap_or_else(|| crc32fast::hash(&e.data));
        let (method, comp) = if e.deflate { (8u16, miniz_oxide::deflate::compress_to_vec(&e.data, 6)) } else { (0u16, e.data.clone()) };
        let usz = e.usize_override.unwrap_or(e.data.len() as u32);
        let off = out.len() as u32;
        let n = e.name.as_bytes();
        let mut lh = Vec::new();
        lh.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        lh.extend_from_slice(&20u16.to_le_bytes());
        lh.extend_from_slice(&e.flags.to_le_bytes());
        lh.extend_from_slice(&method.to_le_bytes());
        lh.extend_from_slice(&[0, 0, 0, 0]);
        lh.extend_from_slice(&crc.to_le_bytes());
        lh.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        lh.extend_from_slice(&usz.to_le_bytes());
        lh.extend_from_slice(&(n.len() as u16).to_le_bytes());
        lh.extend_from_slice(&0u16.to_le_bytes());
        lh.extend_from_slice(n);
        out.extend_from_slice(&lh);
        out.extend_from_slice(&comp);
        cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&e.flags.to_le_bytes());
        cd.extend_from_slice(&method.to_le_bytes());
        cd.extend_from_slice(&[0, 0, 0, 0]);
        cd.extend_from_slice(&crc.to_le_bytes());
        cd.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        cd.extend_from_slice(&usz.to_le_bytes());
        cd.extend_from_slice(&(n.len() as u16).to_le_bytes());
        cd.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        cd.extend_from_slice(&off.to_le_bytes());
        cd.extend_from_slice(n);
    }
    let cd_off = out.len() as u32;
    out.extend_from_slice(&cd);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_off.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

const TOP: &str = "HoldMyRAM-0.2.0-win-x64";

fn good_zip() -> Vec<u8> {
    make_zip(&[
        ZEntry { name: "HoldMyRAM-0.2.0-win-x64/", data: vec![], deflate: false, usize_override: None, flags: 0, crc_override: None },
        ze("HoldMyRAM-0.2.0-win-x64/holdmyram.exe", &fake_pe("main-0.2.0")),
        ze("HoldMyRAM-0.2.0-win-x64\\holdmyram-helper.exe", &fake_pe("helper-0.2.0")),
        ze("HoldMyRAM-0.2.0-win-x64/README.txt", b"read me"),
        ZEntry { deflate: false, ..ze("HoldMyRAM-0.2.0-win-x64/Start here.txt", b"start") },
    ])
}

/// Fake network: URL -> response; records requests.
#[derive(Default)]
struct FakeNet {
    routes: HashMap<String, Result<HttpResponse, NetError>>,
    seen: Mutex<Vec<String>>,
}

impl FakeNet {
    fn ok(mut self, url: &str, body: &[u8]) -> Self {
        self.routes.insert(url.into(), Ok(HttpResponse { status: 200, headers: vec![], body: body.to_vec() }));
        self
    }
    fn status(mut self, url: &str, status: u16, headers: &[(&str, &str)]) -> Self {
        let headers = headers.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        self.routes.insert(url.into(), Ok(HttpResponse { status, headers, body: vec![] }));
        self
    }
    fn err(mut self, url: &str, offline: bool) -> Self {
        self.routes.insert(url.into(), Err(NetError { offline, msg: "boom".into() }));
        self
    }
}

impl Http for FakeNet {
    fn get(&self, url: &Url, _accept: &str, max: u64, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<HttpResponse, NetError> {
        let key = format!("{}://{}{}{}", if url.https { "https" } else { "http" }, url.host, if url.port == 443 || url.port == 80 { String::new() } else { format!(":{}", url.port) }, url.path);
        self.seen.lock().unwrap().push(key.clone());
        let r = self.routes.get(&key).cloned().unwrap_or(Ok(HttpResponse { status: 404, headers: vec![], body: vec![] }))?;
        if r.body.len() as u64 > max {
            return Err(NetError { offline: false, msg: "too large".into() });
        }
        progress(r.body.len() as u64, Some(r.body.len() as u64));
        Ok(r)
    }
}

const API: &str = "https://api.github.com/repos/holdmysocks/HoldMyRAM/releases/latest";
const ZIP_URL: &str = "https://github.com/holdmysocks/HoldMyRAM/releases/download/v0.2.0/HoldMyRAM-0.2.0-win-x64.zip";
const SUMS_URL: &str = "https://github.com/holdmysocks/HoldMyRAM/releases/download/v0.2.0/SHA256SUMS.txt";

fn release_json(tag: &str, assets: &[(&str, &str)], body: &str, draft: bool) -> String {
    let a: Vec<String> = assets.iter().map(|(n, u)| format!(r#"{{"name":"{n}","browser_download_url":"{u}","size":1234}}"#)).collect();
    format!(
        r#"{{"tag_name":"{tag}","html_url":"https://github.com/holdmysocks/HoldMyRAM/releases/tag/{tag}","body":{},"draft":{draft},"prerelease":false,"assets":[{}]}}"#,
        serde_json::to_string(body).unwrap(),
        a.join(",")
    )
}

fn both_assets() -> Vec<(&'static str, &'static str)> {
    vec![("HoldMyRAM-0.2.0-win-x64.zip", ZIP_URL), ("SHA256SUMS.txt", SUMS_URL)]
}

// ------------------------------------------------------------------- versions

#[test]
fn version_and_tag_parsing() {
    assert_eq!(Version::parse_tag("v1.2.3"), Some(Version { major: 1, minor: 2, patch: 3 }));
    assert_eq!(Version::parse_tag("1.2.3"), Some(v("1.2.3")));
    assert_eq!(Version::parse_tag(" V0.1.0 "), Some(v("0.1.0")));
    for bad in ["v1.2.3-beta", "v1.2.3-rc.1", "v1.2.3+build", "v1.2", "v1.2.3.4", "v01.2.3", "v1..3", "", "v", "vx.y.z", "v-1.2.3", "vv1.2.3", "1.2.3a"] {
        assert!(Version::parse_tag(bad).is_none(), "{bad:?} accepted");
    }
    assert!(v("0.10.0") > v("0.9.0"));
    assert!(v("1.0.0") > v("0.99.99"));
    assert!(v("0.1.1") > v("0.1.0"));
    assert_eq!(v("0.10.0").to_string(), "0.10.0");
    assert_eq!(zip_name(&v("0.2.0")), "HoldMyRAM-0.2.0-win-x64.zip");
}

// ------------------------------------------------------------------- sums

#[test]
fn sums_parsing() {
    let h = "a".repeat(64);
    let up = "ABCDEF0123456789".repeat(4);
    let text = format!("{h}  HoldMyRAM-0.2.0-win-x64.zip\r\n{up} *other.zip\r\n\r\nnot a line\n# comment\n");
    assert_eq!(parse_sums(&text, "HoldMyRAM-0.2.0-win-x64.zip").unwrap(), h);
    assert_eq!(parse_sums(&text, "other.zip").unwrap(), up.to_lowercase());
    let e = parse_sums(&text, "missing.zip").unwrap_err();
    assert_eq!(e.code, "bad_checksum");
    assert!(e.msg.contains("not listed"));
    // Short or non-hex hashes are ignored; a conflicting duplicate is an error.
    assert!(parse_sums(&format!("{}  x.zip\n", "a".repeat(63)), "x.zip").is_err());
    assert!(parse_sums(&format!("{}  x.zip\n", "g".repeat(64)), "x.zip").is_err());
    assert!(parse_sums(&format!("{h}  x.zip\n{}  x.zip\n", "b".repeat(64)), "x.zip").is_err());
    assert!(parse_sums(&format!("{h}  x.zip\n{h}  x.zip\n"), "x.zip").is_ok());
    // A name is matched exactly, not as a suffix.
    assert!(parse_sums(&format!("{h}  evil-x.zip\n"), "x.zip").is_err());
}

// ------------------------------------------------------------------- URLs

#[test]
fn release_policy_rejects_other_hosts_and_http() {
    let p = UrlPolicy::RELEASE;
    p.check(API, UrlRole::Api).unwrap();
    p.check(ZIP_URL, UrlRole::Asset).unwrap();
    for (url, role) in [
        ("http://api.github.com/repos/holdmysocks/HoldMyRAM/releases/latest", UrlRole::Api),
        ("http://127.0.0.1:8080/repos/holdmysocks/HoldMyRAM/releases/latest", UrlRole::Api),
        ("https://api.github.com:8443/repos/holdmysocks/HoldMyRAM/releases/latest", UrlRole::Api),
        ("https://api.github.com/repos/someone/else/releases/latest", UrlRole::Api),
        ("https://evil.com/repos/holdmysocks/HoldMyRAM/releases/latest", UrlRole::Api),
        ("https://api.github.com.evil.com/repos/holdmysocks/HoldMyRAM/releases/latest", UrlRole::Api),
        ("https://user@api.github.com/repos/holdmysocks/HoldMyRAM/releases/latest", UrlRole::Api),
        ("http://github.com/holdmysocks/HoldMyRAM/releases/download/v0.2.0/x.zip", UrlRole::Asset),
        ("https://github.com/someone/else/releases/download/v0.2.0/x.zip", UrlRole::Asset),
        ("https://objects.githubusercontent.com/x", UrlRole::Asset),
        ("http://127.0.0.1:9/x.zip", UrlRole::Asset),
        ("https://githubusercontent.com.evil.com/x", UrlRole::Redirect),
        ("https://evilgithubusercontent.com/x", UrlRole::Redirect),
        ("http://objects.githubusercontent.com/x", UrlRole::Redirect),
        ("http://127.0.0.1:9/x", UrlRole::Redirect),
        ("https://evil.com/", UrlRole::Page),
        ("ftp://github.com/x", UrlRole::Page),
        ("https://github.com\\@evil.com/", UrlRole::Page),
    ] {
        assert!(p.check(url, role).is_err(), "{url} accepted as {role:?}");
    }
    for ok in ["https://objects.githubusercontent.com/github-production-release-asset/x?sig=1", "https://release-assets.githubusercontent.com/x", "https://github.com/x"] {
        p.check(ok, UrlRole::Redirect).unwrap();
    }
    p.check("https://github.com/holdmysocks/HoldMyRAM/releases/tag/v1.0.0", UrlRole::Page).unwrap();

    // The release configuration ignores the override entirely.
    let (url, pol) = select_api(Some("http://127.0.0.1:1234/x".into()), false);
    assert_eq!((url.as_str(), pol), (API, UrlPolicy::RELEASE));
    let (url, pol) = select_api(None, true);
    assert_eq!((url.as_str(), pol), (API, UrlPolicy::RELEASE));
    // In this (debug) test build the active policy is RELEASE unless the variable is set.
    if std::env::var_os("HOLDMYRAM_UPDATE_API_URL").is_none() {
        assert_eq!(active_api(), (API.to_string(), UrlPolicy::RELEASE));
    }
}

#[test]
fn debug_policy_allows_only_local_http() {
    let (url, p) = select_api(Some("http://127.0.0.1:5000/api".into()), true);
    assert_eq!(url, "http://127.0.0.1:5000/api");
    p.check(&url, UrlRole::Api).unwrap();
    p.check("http://127.0.0.1:5000/a.zip", UrlRole::Asset).unwrap();
    p.check("http://127.0.0.1:5000/b", UrlRole::Redirect).unwrap();
    assert!(p.check("http://localhost:5000/a.zip", UrlRole::Asset).is_err());
    assert!(p.check("http://10.0.0.1/a.zip", UrlRole::Asset).is_err());
    assert!(p.check("https://evil.com/a.zip", UrlRole::Asset).is_err());
    assert!(p.check("http://127.0.0.1:5000/", UrlRole::Page).is_err(), "pages stay github-only");
}

#[test]
fn redirects_are_checked_hop_by_hop() {
    let cdn = "https://objects.githubusercontent.com/asset?x=1";
    let net = FakeNet::default().status(ZIP_URL, 302, &[("location", cdn)]).ok(cdn, b"data");
    let r = fetch(&net, UrlPolicy::RELEASE, ZIP_URL, UrlRole::Asset, "*/*", 100, &mut |_, _| {}).unwrap();
    assert_eq!(r.body, b"data");
    let net = FakeNet::default().status(ZIP_URL, 302, &[("location", "https://evil.com/x")]);
    let e = fetch(&net, UrlPolicy::RELEASE, ZIP_URL, UrlRole::Asset, "*/*", 100, &mut |_, _| {}).unwrap_err();
    assert_eq!(e.code, "bad_url");
    assert_eq!(net.seen.lock().unwrap().len(), 1, "the evil host was never contacted");
    let net = FakeNet::default().status(ZIP_URL, 302, &[("location", "http://objects.githubusercontent.com/x")]);
    assert_eq!(fetch(&net, UrlPolicy::RELEASE, ZIP_URL, UrlRole::Asset, "*/*", 100, &mut |_, _| {}).unwrap_err().code, "bad_url");
    // A loop ends.
    let net = FakeNet::default().status(ZIP_URL, 302, &[("location", ZIP_URL)]);
    assert!(fetch(&net, UrlPolicy::RELEASE, ZIP_URL, UrlRole::Asset, "*/*", 100, &mut |_, _| {}).unwrap_err().msg.contains("too many"));
    // A relative redirect stays on the same host.
    let net = FakeNet::default().status(ZIP_URL, 302, &[("location", "/holdmysocks/HoldMyRAM/x")]).ok("https://github.com/holdmysocks/HoldMyRAM/x", b"ok");
    assert_eq!(fetch(&net, UrlPolicy::RELEASE, ZIP_URL, UrlRole::Asset, "*/*", 100, &mut |_, _| {}).unwrap().body, b"ok");
}

// ------------------------------------------------------------------- check

#[test]
fn check_outcomes() {
    let cur = v("0.1.0");
    let json = release_json("v0.2.0", &both_assets(), "## What's new\r\n- faster", false);
    let rec = check(&FakeNet::default().ok(API, json.as_bytes()), API, UrlPolicy::RELEASE, &cur, 100);
    assert_eq!(rec.latest.as_deref(), Some("0.2.0"));
    assert!(rec.available(&cur) && rec.installable(&cur) && rec.error.is_none());
    assert_eq!(rec.notes.as_deref(), Some("## What's new\n- faster"));
    assert_eq!(rec.release_url.as_deref(), Some("https://github.com/holdmysocks/HoldMyRAM/releases/tag/v0.2.0"));
    assert_eq!(rec.zip.as_ref().unwrap().url, ZIP_URL);
    assert_eq!(rec.checked_at, 100);
    // Same version: not available, no asset needed.
    let json = release_json("v0.1.0", &[], "", false);
    let rec = check(&FakeNet::default().ok(API, json.as_bytes()), API, UrlPolicy::RELEASE, &cur, 1);
    assert!(!rec.available(&cur) && rec.error.is_none());
    // Missing sums: available but not installable, no_asset recorded.
    let json = release_json("v0.2.0", &[("HoldMyRAM-0.2.0-win-x64.zip", ZIP_URL)], "", false);
    let rec = check(&FakeNet::default().ok(API, json.as_bytes()), API, UrlPolicy::RELEASE, &cur, 1);
    assert!(rec.available(&cur) && !rec.installable(&cur));
    assert_eq!(rec.error_code.as_deref(), Some("no_asset"));
    // Wrongly named zip.
    let json = release_json("v0.2.0", &[("HoldMyRAM-0.2.0.zip", ZIP_URL), ("SHA256SUMS.txt", SUMS_URL)], "", false);
    assert_eq!(check(&FakeNet::default().ok(API, json.as_bytes()), API, UrlPolicy::RELEASE, &cur, 1).error_code.as_deref(), Some("no_asset"));
    // Asset on another host.
    let json = release_json("v0.2.0", &[("HoldMyRAM-0.2.0-win-x64.zip", "https://evil.com/a.zip"), ("SHA256SUMS.txt", SUMS_URL)], "", false);
    assert_eq!(check(&FakeNet::default().ok(API, json.as_bytes()), API, UrlPolicy::RELEASE, &cur, 1).error_code.as_deref(), Some("no_asset"));
    // Draft / pre-release flags: treated as nothing.
    let json = release_json("v0.2.0", &both_assets(), "", true);
    let rec = check(&FakeNet::default().ok(API, json.as_bytes()), API, UrlPolicy::RELEASE, &cur, 1);
    assert!(rec.latest.is_none() && !rec.available(&cur));
    // Pre-release tag.
    let json = release_json("v0.2.0-beta", &both_assets(), "", false);
    assert_eq!(check(&FakeNet::default().ok(API, json.as_bytes()), API, UrlPolicy::RELEASE, &cur, 1).error_code.as_deref(), Some("bad_release"));
    // Huge body trimmed.
    let json = release_json("v0.2.0", &both_assets(), &"x".repeat(10_000), false);
    let rec = check(&FakeNet::default().ok(API, json.as_bytes()), API, UrlPolicy::RELEASE, &cur, 1);
    assert_eq!(rec.notes.as_ref().unwrap().chars().count(), MAX_NOTES_CHARS);
    // Garbage JSON.
    assert_eq!(check(&FakeNet::default().ok(API, b"<html>"), API, UrlPolicy::RELEASE, &cur, 1).error_code.as_deref(), Some("bad_release"));
}

#[test]
fn check_failures_mean_no_update() {
    let cur = v("0.1.0");
    let code = |net: FakeNet| {
        let r = check(&net, API, UrlPolicy::RELEASE, &cur, 5);
        assert!(!r.available(&cur) && r.latest.is_none() && r.error.is_some() && r.checked_at == 5);
        r.error_code.unwrap()
    };
    // The repo is private today: 404 reads as "no release", not a bug.
    assert_eq!(code(FakeNet::default().status(API, 404, &[])), "not_found");
    assert_eq!(code(FakeNet::default().status(API, 403, &[("x-ratelimit-remaining", "0")])), "rate_limited");
    assert_eq!(code(FakeNet::default().status(API, 429, &[])), "rate_limited");
    assert_eq!(code(FakeNet::default().status(API, 403, &[])), "forbidden");
    assert_eq!(code(FakeNet::default().status(API, 500, &[])), "http_error");
    assert_eq!(code(FakeNet::default().err(API, true)), "offline");
    assert_eq!(code(FakeNet::default().err(API, false)), "network");
    // The release policy never calls a non-GitHub API URL.
    let net = FakeNet::default();
    assert_eq!(check(&net, "https://evil.com/x", UrlPolicy::RELEASE, &cur, 1).error_code.as_deref(), Some("bad_url"));
    assert!(net.seen.lock().unwrap().is_empty());
}

// ------------------------------------------------------------------- cache

#[test]
fn cache_behaviour() {
    let dir = temp_dir("update-cache");
    assert_eq!(load_cache(&dir), CacheFile::default());
    let rec = CheckRecord { checked_at: 1000, latest: Some("0.2.0".into()), ..Default::default() };
    assert!(rec.fresh(1000) && rec.fresh(1000 + CACHE_SECS - 1));
    assert!(!rec.fresh(1000 + CACHE_SECS));
    assert!(!rec.fresh(999), "a clock that went back counts as stale");
    let c = CacheFile { last: Some(rec.clone()), updated: Some("updated from 0.1.0 to 0.2.0".into()) };
    save_cache(&dir, &c).unwrap();
    assert_eq!(load_cache(&dir), c);
    // After updating, the same cached record is no longer "available".
    assert!(rec.available(&v("0.1.0")) && !rec.available(&v("0.2.0")));
    std::fs::write(cache_path(&dir), b"{ corrupt").unwrap();
    assert_eq!(load_cache(&dir), CacheFile::default());
}

// ------------------------------------------------------------------- zip

#[test]
fn zip_valid_layouts() {
    let e = z::read_validated(&good_zip(), TOP).unwrap();
    let names: Vec<_> = e.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["holdmyram.exe", "holdmyram-helper.exe", "README.txt", "Start here.txt"]);
    assert_eq!(e[0].data, fake_pe("main-0.2.0"));
    // Flat layout, any case.
    let flat = make_zip(&[ze("HOLDMYRAM.EXE", &fake_pe("a")), ze("holdmyram-helper.exe", &fake_pe("b"))]);
    let e = z::read_validated(&flat, TOP).unwrap();
    assert_eq!(e[0].name, "holdmyram.exe");
}

#[test]
fn zip_rejects_bad_names_and_shapes() {
    let pe = fake_pe("x");
    for name in [
        "../holdmyram.exe",
        "/holdmyram.exe",
        "\\holdmyram.exe",
        "C:/holdmyram.exe",
        "C:holdmyram.exe",
        "holdmyram.exe:evil",
        "HoldMyRAM-0.2.0-win-x64/../holdmyram.exe",
        "HoldMyRAM-0.2.0-win-x64/sub/holdmyram.exe",
        "HoldMyRAM-0.1.0-win-x64/holdmyram.exe",
        "other/holdmyram.exe",
        "evil.dll",
        "holdmyram.exe.config",
        "HoldMyRAM-0.2.0-win-x64/",
        "HoldMyRAM-0.2.0-win-x64//holdmyram.exe",
        "hold\u{1}myram.exe",
    ] {
        let zip = make_zip(&[ze(name, &pe), ze("README.txt", b"x")]);
        let r = z::read_validated(&zip, TOP);
        // The top folder alone is fine only as an empty folder entry.
        if name == "HoldMyRAM-0.2.0-win-x64/" {
            assert!(r.unwrap_err().msg.contains("has data"), "{name}");
        } else {
            assert_eq!(r.unwrap_err().code, "bad_archive", "{name}");
        }
    }
    // Duplicates (also across layouts).
    let dup = make_zip(&[ze("README.txt", b"a"), ze("HoldMyRAM-0.2.0-win-x64/readme.TXT", b"b")]);
    assert!(z::read_validated(&dup, TOP).unwrap_err().msg.contains("duplicate"));
    // Too many entries.
    let many: Vec<ZEntry> = (0..=z::MAX_ENTRIES).map(|_| ZEntry { name: "HoldMyRAM-0.2.0-win-x64/", data: vec![], deflate: false, usize_override: None, flags: 0, crc_override: None }).collect();
    assert!(z::read_validated(&make_zip(&many), TOP).unwrap_err().msg.contains("too many"));
    // Claims to expand beyond the cap (checked before decompressing).
    let big = make_zip(&[ZEntry { usize_override: Some((z::MAX_TOTAL + 1) as u32), ..ze("holdmyram.exe", &pe) }]);
    assert!(z::read_validated(&big, TOP).unwrap_err().msg.contains("expands beyond"));
    // Lies about its size.
    let liar = make_zip(&[ZEntry { usize_override: Some(10), ..ze("holdmyram.exe", &pe) }]);
    assert!(z::read_validated(&liar, TOP).is_err());
    // Encrypted, bad CRC, not a zip.
    assert!(z::read_validated(&make_zip(&[ZEntry { flags: 1, ..ze("README.txt", b"x") }]), TOP).unwrap_err().msg.contains("encrypted"));
    assert!(z::read_validated(&make_zip(&[ZEntry { crc_override: Some(1), ..ze("README.txt", b"x") }]), TOP).unwrap_err().msg.contains("CRC"));
    assert!(z::read_validated(b"PK not really", TOP).is_err());
    assert!(z::read_validated(&[], TOP).is_err());
    let mut cut = good_zip();
    cut.truncate(cut.len() / 2);
    assert!(z::read_validated(&cut, TOP).is_err());
}

#[test]
fn pe_check() {
    install::check_pe_x64(&fake_pe("ok")).unwrap();
    assert!(install::check_pe_x64(b"MZ").is_err());
    assert!(install::check_pe_x64(b"#!/bin/sh echo hi, this is long enough to pass the size check......").is_err());
    let mut x86 = fake_pe("x");
    x86[0x84..0x86].copy_from_slice(&0x14cu16.to_le_bytes());
    assert!(install::check_pe_x64(&x86).unwrap_err().contains("x64"));
    let mut pe32 = fake_pe("x");
    pe32[0x98..0x9a].copy_from_slice(&0x10Bu16.to_le_bytes());
    assert!(install::check_pe_x64(&pe32).is_err());
    let mut far = fake_pe("x");
    far[0x3c..0x40].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
    assert!(install::check_pe_x64(&far).is_err());
    // The real exes of this build pass.
    let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    install::check_pe_x64(&me).unwrap();
}

#[test]
fn stage_rejects_archives_without_valid_exes() {
    let dir = temp_dir("update-stage-bad");
    let no_helper = make_zip(&[ze("holdmyram.exe", &fake_pe("a"))]);
    assert!(stage_zip(&no_helper, &v("0.2.0"), &dir).unwrap_err().msg.contains("holdmyram-helper.exe"));
    let not_pe = make_zip(&[ze("holdmyram.exe", b"MZ but nothing else at all, definitely not a PE file........"), ze("holdmyram-helper.exe", &fake_pe("b"))]);
    assert_eq!(stage_zip(&not_pe, &v("0.2.0"), &dir).unwrap_err().code, "bad_archive");
}

// ------------------------------------------------------------------- download

fn sums_for(zip: &[u8]) -> String {
    format!("{}  HoldMyRAM-0.2.0-win-x64.zip\r\n", sha256_hex(zip).unwrap().to_uppercase())
}

fn installable_record() -> CheckRecord {
    let json = release_json("v0.2.0", &both_assets(), "", false);
    check(&FakeNet::default().ok(API, json.as_bytes()), API, UrlPolicy::RELEASE, &v("0.1.0"), 1)
}

#[test]
fn download_verify_and_stage() {
    let dir = temp_dir("update-dl");
    let zip = good_zip();
    let net = FakeNet::default().ok(SUMS_URL, sums_for(&zip).as_bytes()).ok(ZIP_URL, &zip);
    let mut ticks = 0;
    let s = download_and_stage(&net, UrlPolicy::RELEASE, &installable_record(), &v("0.1.0"), &dir, &mut |_, _| ticks += 1).unwrap();
    assert!(ticks > 0);
    assert_eq!(s.version, "0.2.0");
    assert_eq!(s.dir, dir.join("0.2.0").join("staged"));
    assert!(s.dir.join("holdmyram.exe").exists() && s.dir.join("Start here.txt").exists());
    assert_eq!(load_staged(&dir, &v("0.2.0")).unwrap(), s);
    // Tampering after staging is noticed.
    std::fs::write(s.dir.join("holdmyram.exe"), fake_pe("tampered")).unwrap();
    assert!(load_staged(&dir, &v("0.2.0")).is_none());
    // Pruning keeps only newer versions.
    std::fs::create_dir_all(dir.join("0.1.0")).unwrap();
    prune_updates(&dir, &v("0.1.0"));
    assert!(!dir.join("0.1.0").exists() && dir.join("0.2.0").exists());
}

#[test]
fn download_refuses_bad_checksums() {
    let dir = temp_dir("update-dl-bad");
    let zip = good_zip();
    let rec = installable_record();
    let run = |net: FakeNet| download_and_stage(&net, UrlPolicy::RELEASE, &rec, &v("0.1.0"), &dir, &mut |_, _| {}).unwrap_err();
    let wrong = format!("{}  HoldMyRAM-0.2.0-win-x64.zip\n", "0".repeat(64));
    assert_eq!(run(FakeNet::default().ok(SUMS_URL, wrong.as_bytes()).ok(ZIP_URL, &zip)).code, "bad_checksum");
    let unlisted = format!("{}  something-else.zip\n", sha256_hex(&zip).unwrap());
    let e = run(FakeNet::default().ok(SUMS_URL, unlisted.as_bytes()).ok(ZIP_URL, &zip));
    assert!(e.code == "bad_checksum" && e.msg.contains("not listed"));
    assert_eq!(run(FakeNet::default().ok(ZIP_URL, &zip)).code, "not_found", "sums missing");
    // A zip that is not newer is refused before any download.
    let net = FakeNet::default();
    assert_eq!(download_and_stage(&net, UrlPolicy::RELEASE, &rec, &v("0.2.0"), &dir, &mut |_, _| {}).unwrap_err().code, "no_update");
    assert!(net.seen.lock().unwrap().is_empty());
    assert!(!dir.join("0.2.0").join("staged").exists());
}

// ------------------------------------------------------------------- swap and install

fn staged_in(dir: &std::path::Path) -> Staged {
    let zip = good_zip();
    let net = FakeNet::default().ok(SUMS_URL, sums_for(&zip).as_bytes()).ok(ZIP_URL, &zip);
    download_and_stage(&net, UrlPolicy::RELEASE, &installable_record(), &v("0.1.0"), &dir.join("updates"), &mut |_, _| {}).unwrap()
}

fn app_with_old(dir: &std::path::Path) -> std::path::PathBuf {
    let app = dir.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("holdmyram.exe"), fake_pe("main-0.1.0")).unwrap();
    std::fs::write(app.join("holdmyram-helper.exe"), fake_pe("helper-0.1.0")).unwrap();
    app
}

#[test]
fn swap_success_and_cleanup() {
    let dir = temp_dir("update-swap-ok");
    let s = staged_in(&dir);
    let app = app_with_old(&dir);
    let done = install::swap(&app, &s).unwrap();
    assert_eq!(std::fs::read(app.join("holdmyram.exe")).unwrap(), fake_pe("main-0.2.0"));
    assert_eq!(std::fs::read(app.join("holdmyram-helper.exe")).unwrap(), fake_pe("helper-0.2.0"));
    assert_eq!(std::fs::read(app.join("holdmyram.exe.old")).unwrap(), fake_pe("main-0.1.0"));
    assert!(app.join("README.txt").exists());
    assert_eq!(done.files.len(), 2);
    // The next start removes the leftovers.
    let removed = install::cleanup_old(&app);
    assert_eq!(removed.len(), 2);
    assert!(!app.join("holdmyram.exe.old").exists() && !app.join("holdmyram-helper.exe.old").exists());
    assert!(app.join("holdmyram.exe").exists());
    // Rollback of a completed swap restores the old files.
    let app2 = app_with_old(&dir.join("second"));
    let done = install::swap(&app2, &s).unwrap();
    assert!(install::rollback(&done).is_empty());
    assert_eq!(std::fs::read(app2.join("holdmyram.exe")).unwrap(), fake_pe("main-0.1.0"));
    assert_eq!(std::fs::read(app2.join("holdmyram-helper.exe")).unwrap(), fake_pe("helper-0.1.0"));
    assert!(!app2.join("holdmyram.exe.old").exists());
}

#[test]
fn swap_failure_on_second_file_rolls_back() {
    let dir = temp_dir("update-swap-fail");
    let s = staged_in(&dir);
    let app = app_with_old(&dir);
    // A non-empty FOLDER where the helper's .old must go: removing and renaming fail.
    std::fs::create_dir_all(app.join("holdmyram-helper.exe.old").join("blocker")).unwrap();
    let e = install::swap(&app, &s).unwrap_err();
    assert_eq!(e.code, "swap_failed");
    assert!(e.msg.contains("everything was put back"), "{}", e.msg);
    assert_eq!(std::fs::read(app.join("holdmyram.exe")).unwrap(), fake_pe("main-0.1.0"));
    assert_eq!(std::fs::read(app.join("holdmyram-helper.exe")).unwrap(), fake_pe("helper-0.1.0"));
    assert!(!app.join("holdmyram.exe.old").exists() && !app.join("holdmyram.exe.new").exists());
}

#[test]
fn swap_refuses_a_tampered_stage() {
    let dir = temp_dir("update-swap-tamper");
    let s = staged_in(&dir);
    let app = app_with_old(&dir);
    std::fs::write(s.dir.join("holdmyram-helper.exe"), fake_pe("evil")).unwrap();
    assert_eq!(install::swap(&app, &s).unwrap_err().code, "bad_archive");
    assert_eq!(std::fs::read(app.join("holdmyram.exe")).unwrap(), fake_pe("main-0.1.0"));
    assert!(!app.join("holdmyram.exe.old").exists());
}

struct FakeHost {
    restore: RestoreOutcome,
    launch_ok: bool,
    log: Vec<String>,
}

impl InstallHost for FakeHost {
    fn restore(&mut self) -> RestoreOutcome {
        self.log.push("restore".into());
        self.restore.clone()
    }
    fn stop_helper(&mut self) {
        self.log.push("stop_helper".into());
    }
    fn launch(&mut self, exe: &std::path::Path, old: &str) -> Result<(), String> {
        self.log.push(format!("launch {} --updated-from {old}", exe.file_name().unwrap().to_string_lossy()));
        if self.launch_ok {
            Ok(())
        } else {
            Err("could not start".into())
        }
    }
}

fn host(restore: RestoreOutcome, launch_ok: bool) -> FakeHost {
    FakeHost { restore, launch_ok, log: Vec::new() }
}

#[test]
fn install_flow() {
    let dir = temp_dir("update-install");
    let s = staged_in(&dir);

    // Restore needs admin (declined): abort, nothing touched, helper left alone.
    let app = app_with_old(&dir.join("a"));
    let mut h = host(RestoreOutcome::NeedsAdmin, true);
    let e = install::install(&mut h, &app, &s, "0.1.0").unwrap_err();
    assert_eq!(e.code, "restore_needed_admin");
    assert_eq!(h.log, vec!["restore"]);
    assert_eq!(std::fs::read(app.join("holdmyram.exe")).unwrap(), fake_pe("main-0.1.0"));
    assert!(!app.join("holdmyram.exe.old").exists());

    let mut h = host(RestoreOutcome::Failed("2 apps".into()), true);
    assert_eq!(install::install(&mut h, &app, &s, "0.1.0").unwrap_err().code, "restore_failed");

    // Success: restore, stop helper, swap, launch the new exe with --updated-from.
    let mut h = host(RestoreOutcome::Restored, true);
    install::install(&mut h, &app, &s, "0.1.0").unwrap();
    assert_eq!(h.log, vec!["restore", "stop_helper", "launch holdmyram.exe --updated-from 0.1.0"]);
    assert_eq!(std::fs::read(app.join("holdmyram.exe")).unwrap(), fake_pe("main-0.2.0"));

    // Launch failure: rolled back.
    let app = app_with_old(&dir.join("b"));
    let mut h = host(RestoreOutcome::NotNeeded, false);
    let e = install::install(&mut h, &app, &s, "0.1.0").unwrap_err();
    assert_eq!(e.code, "launch_failed");
    assert!(e.msg.contains("put back"));
    assert_eq!(std::fs::read(app.join("holdmyram.exe")).unwrap(), fake_pe("main-0.1.0"));

    // Not newer, and a folder we cannot write to.
    let mut h = host(RestoreOutcome::NotNeeded, true);
    assert_eq!(install::install(&mut h, &app, &s, "0.2.0").unwrap_err().code, "no_update");
    assert_eq!(install::install(&mut h, &dir.join("does-not-exist"), &s, "0.1.0").unwrap_err().code, "not_writable");
    assert!(h.log.is_empty(), "nothing restored or launched");
    assert!(install::dir_writable(&dir));
}

// ------------------------------------------------------------------- manual

/// Real network, read-only: `cargo test -p holdmyram-core update_real_github -- --ignored --nocapture`.
/// While the repository is private this prints not_found.
#[test]
#[ignore]
fn update_real_github() {
    let rec = check(&http::WinHttp, API, UrlPolicy::RELEASE, &v("0.0.1"), now_unix());
    println!("{rec:#?}");
}

/// Against a local server (scripts\test-update-local.ps1 -ServeOnly prints the URL):
/// `$env:HOLDMYRAM_UPDATE_API_URL="http://127.0.0.1:PORT/repos/holdmysocks/HoldMyRAM/releases/latest"`
/// then `cargo test -p holdmyram-core update_local_server -- --ignored --nocapture`.
#[test]
#[ignore]
fn update_local_server() {
    let (url, policy) = active_api();
    assert!(policy.allow_local_http, "set HOLDMYRAM_UPDATE_API_URL first");
    let cur = v(env!("CARGO_PKG_VERSION"));
    let rec = check(&http::WinHttp, &url, policy, &cur, now_unix());
    println!("{rec:#?}");
    assert!(rec.installable(&cur), "{rec:?}");
    let dir = temp_dir("update-local");
    let s = download_and_stage(&http::WinHttp, policy, &rec, &cur, &dir, &mut |d, t| println!("{d}/{t:?}")).unwrap();
    println!("staged: {s:#?}");
}

#[test]
fn zip_reader_never_panics_on_damaged_input() {
    let good = good_zip();
    for n in 0..good.len() {
        let _ = z::read_validated(&good[..n], TOP);
    }
    // Flip every byte in turn (deterministic): errors are fine, panics are not.
    for i in 0..good.len() {
        let mut b = good.clone();
        b[i] ^= 0xA5;
        let _ = z::read_validated(&b, TOP);
    }
}

// ------------------------------------------------------------------- handle guards and the worker

static CLOSED: Mutex<Vec<usize>> = Mutex::new(Vec::new());
fn record_close(h: *mut core::ffi::c_void) {
    CLOSED.lock().unwrap().push(h as usize);
}

#[test]
fn handle_guards_close_every_handle_in_reverse_order_on_every_path() {
    use super::http::Guard;
    fn work(fail_at: usize) -> Result<(), &'static str> {
        let session = Guard::new(1 as *mut _, record_close);
        if fail_at == 1 {
            return Err("after session");
        }
        let conn = Guard::new(2 as *mut _, record_close);
        if fail_at == 2 {
            return Err("after connect");
        }
        let req = Guard::new(3 as *mut _, record_close);
        let _null = Guard::new(std::ptr::null_mut(), record_close); // never "closed"
        let _ = (&session, &conn, &req);
        if fail_at == 3 {
            return Err("after request");
        }
        Ok(())
    }
    for (fail_at, want) in [(1, vec![1]), (2, vec![2, 1]), (3, vec![3, 2, 1]), (0, vec![3, 2, 1])] {
        CLOSED.lock().unwrap().clear();
        let _ = work(fail_at);
        assert_eq!(*CLOSED.lock().unwrap(), want, "fail_at {fail_at}");
    }
    // Panics unwind through the guards too.
    CLOSED.lock().unwrap().clear();
    let r = std::panic::catch_unwind(|| {
        let _a = Guard::new(7 as *mut _, record_close);
        panic!("boom");
    });
    assert!(r.is_err());
    assert_eq!(*CLOSED.lock().unwrap(), vec![7]);
}

#[test]
fn worker_protocol_round_trip() {
    use super::worker::{self, Line, Reply, Request};
    let json = release_json("v0.2.0", &both_assets(), "notes", false);
    // Check (the worker uses the RELEASE API URL here: no override in tests).
    if std::env::var_os("HOLDMYRAM_UPDATE_API_URL").is_none() {
        let net = FakeNet::default().ok(API, json.as_bytes());
        let req = serde_json::to_string(&Request::Check { current: "0.1.0".into() }).unwrap() + "\n";
        let mut out = Vec::new();
        assert_eq!(worker::run(&mut req.as_bytes(), &mut out, &net), 0);
        let text = String::from_utf8(out).unwrap();
        let Line::Done(Reply::Checked { record }) = worker::parse_line(text.lines().last().unwrap()) else { panic!("{text}") };
        assert!(record.installable(&v("0.1.0")));

        // Download: progress lines, then the staged result.
        let dir = temp_dir("update-worker");
        let zip = good_zip();
        let net = FakeNet::default().ok(SUMS_URL, sums_for(&zip).as_bytes()).ok(ZIP_URL, &zip);
        let req = serde_json::to_string(&Request::Download { record, current: "0.1.0".into(), updates_dir: dir.clone() }).unwrap() + "\n";
        let mut out = Vec::new();
        worker::run(&mut req.as_bytes(), &mut out, &net);
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<Line> = text.lines().map(worker::parse_line).collect();
        assert!(lines.iter().any(|l| matches!(l, Line::Progress(d, Some(t)) if d == t)), "{text}");
        let Some(Line::Done(Reply::Staged { staged })) = lines.last() else { panic!("{text}") };
        assert_eq!(load_staged(&dir, &v("0.2.0")).as_ref(), Some(staged));
    }
    // Failures come back as a reply, never a crash.
    let mut out = Vec::new();
    assert_eq!(worker::run(&mut "not json\n".as_bytes(), &mut out, &FakeNet::default()), 2);
    assert!(matches!(worker::parse_line(String::from_utf8(out).unwrap().trim()), Line::Done(Reply::Failed { .. })));
    let req = serde_json::to_string(&Request::Download { record: CheckRecord::default(), current: "0.1.0".into(), updates_dir: temp_dir("update-worker-2") }).unwrap() + "\n";
    let mut out = Vec::new();
    worker::run(&mut req.as_bytes(), &mut out, &FakeNet::default());
    assert!(matches!(worker::parse_line(String::from_utf8(out).unwrap().trim()), Line::Done(Reply::Failed { code, .. }) if code == "no_update"));
    assert_eq!(worker::parse_line("P 5 -"), Line::Progress(5, None));
    assert_eq!(worker::parse_line("garbage"), Line::Other);
}

