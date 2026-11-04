//! On-demand inventory of apps and background items. Read only when the UI asks; nothing polls.
//!
//! Strictly read-only: this module opens processes, services, registry keys and scheduled
//! tasks with query rights only and never changes anything. Every source degrades on its
//! own: access denied or a missing key skips that item (or that source), never the scan.
//!
//! `scan_all` runs the slow sources (services over SCM RPC, tasks and .lnk resolution over
//! COM) on scoped threads that are always joined before it returns, so nothing is left
//! running afterwards.

use crate::process::{list_processes, ProcInfo};
use crate::risk::Kind;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// One row in the UI: an installed app, a running process group, a startup entry,
/// a scheduled task or a service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanItem {
    /// Lookup key for the risk database and for selections (exe name for processes,
    /// service short name, task name, startup entry name, app display name).
    pub name: String,
    /// Friendly label for the UI.
    pub display: String,
    pub kind: Kind,
    /// Currently running (processes/services), or enabled (startup entries/tasks).
    pub running: bool,
    /// Combined working set of all its processes; 0 if not applicable.
    pub ram_bytes: u64,
    pub pids: Vec<u32>,
    /// Extra facts for the UI: path, publisher, start type, task folder, etc.
    pub detail: String,
}

/// Inventory everything: process groups, services, startup entries, scheduled tasks and
/// installed apps, in that order. Read on demand; returns when every source is done.
pub fn scan_all() -> Vec<ScanItem> {
    std::thread::scope(|s| {
        let services = s.spawn(services::enumerate);
        let com = s.spawn(|| {
            let _com = com::Apartment::enter();
            let startup = startup::scan();
            let tasks = tasks::scan();
            (startup, tasks)
        });

        let procs = list_processes();
        let apps = apps::scan();

        let mut out = processes::scan(&procs);
        let ws: HashMap<u32, u64> = procs.iter().map(|p| (p.pid, p.working_set_bytes)).collect();
        if let Ok(raw) = services.join() {
            out.extend(services::to_items(raw, &ws));
        }
        if let Ok((startup, tasks)) = com.join() {
            out.extend(startup);
            out.extend(tasks);
        }
        out.extend(apps);
        out
    })
}

// ---------------------------------------------------------------------------------------
// Small Win32 helpers
// ---------------------------------------------------------------------------------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16 up to the first NUL.
fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Registry string data (bytes of UTF-16, maybe NUL-terminated, maybe odd-length).
fn reg_bytes_to_string(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    from_wide(&units)
}

fn join_detail(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" | ")
}

mod reg {
    //! Read-only registry access (KEY_READ only, always the native 64-bit view).
    use super::{from_wide, reg_bytes_to_string, wide};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW, HKEY,
        KEY_READ, KEY_WOW64_64KEY, REG_DWORD, REG_EXPAND_SZ, REG_SZ, REG_VALUE_TYPE,
    };

    pub use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    pub struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    pub struct Value {
        pub ty: u32,
        pub data: Vec<u8>,
    }

    impl Value {
        pub fn as_string(&self) -> Option<String> {
            (self.ty == REG_SZ.0 || self.ty == REG_EXPAND_SZ.0).then(|| reg_bytes_to_string(&self.data))
        }
        pub fn as_dword(&self) -> Option<u32> {
            (self.ty == REG_DWORD.0 && self.data.len() >= 4)
                .then(|| u32::from_le_bytes([self.data[0], self.data[1], self.data[2], self.data[3]]))
        }
    }

    impl Key {
        pub fn open(root: HKEY, path: &str) -> Option<Key> {
            let p = wide(path);
            let mut h = HKEY::default();
            let r = unsafe { RegOpenKeyExW(root, PCWSTR(p.as_ptr()), Some(0), KEY_READ | KEY_WOW64_64KEY, &mut h) };
            (r == ERROR_SUCCESS).then_some(Key(h))
        }

        pub fn open_sub(&self, name: &str) -> Option<Key> {
            Key::open(self.0, name)
        }

        pub fn subkeys(&self) -> Vec<String> {
            let mut out = Vec::new();
            let mut buf = [0u16; 512];
            for index in 0..100_000u32 {
                let mut len = buf.len() as u32;
                let r = unsafe { RegEnumKeyExW(self.0, index, Some(PWSTR(buf.as_mut_ptr())), &mut len, None, None, None, None) };
                if r == ERROR_SUCCESS {
                    out.push(String::from_utf16_lossy(&buf[..len as usize]));
                } else if r == ERROR_NO_MORE_ITEMS {
                    break;
                }
                // Any other error (e.g. a name over 512 chars): skip that index.
            }
            out
        }

        /// All named values (the unnamed default value is skipped).
        pub fn values(&self) -> Vec<(String, Value)> {
            let mut out = Vec::new();
            let mut name = vec![0u16; 16_384];
            let mut data = vec![0u8; 4096];
            let mut index = 0u32;
            let mut retries = 0;
            while index < 100_000 {
                let mut name_len = name.len() as u32;
                let mut data_len = data.len() as u32;
                let mut ty = 0u32;
                let r = unsafe {
                    RegEnumValueW(
                        self.0,
                        index,
                        Some(PWSTR(name.as_mut_ptr())),
                        &mut name_len,
                        None,
                        Some(&mut ty),
                        Some(data.as_mut_ptr()),
                        Some(&mut data_len),
                    )
                };
                if r == ERROR_MORE_DATA && retries < 3 {
                    data.resize((data_len as usize).max(data.len() * 2), 0);
                    retries += 1;
                    continue;
                }
                retries = 0;
                if r == ERROR_NO_MORE_ITEMS {
                    break;
                }
                if r == ERROR_SUCCESS && name_len > 0 {
                    let n = from_wide(&name[..name_len as usize]);
                    out.push((n, Value { ty, data: data[..data_len as usize].to_vec() }));
                }
                index += 1;
            }
            out
        }

        pub fn value(&self, name: &str) -> Option<Value> {
            let n = wide(name);
            let mut size = 0u32;
            let mut ty = REG_VALUE_TYPE::default();
            for _ in 0..3 {
                let r = unsafe { RegQueryValueExW(self.0, PCWSTR(n.as_ptr()), None, Some(&mut ty), None, Some(&mut size)) };
                if r != ERROR_SUCCESS {
                    return None;
                }
                let mut data = vec![0u8; size as usize];
                let r = unsafe {
                    RegQueryValueExW(self.0, PCWSTR(n.as_ptr()), None, Some(&mut ty), Some(data.as_mut_ptr()), Some(&mut size))
                };
                if r == ERROR_SUCCESS {
                    data.truncate(size as usize);
                    return Some(Value { ty: ty.0, data });
                }
                if r != ERROR_MORE_DATA {
                    return None;
                }
            }
            None
        }

        pub fn string(&self, name: &str) -> Option<String> {
            self.value(name).and_then(|v| v.as_string())
        }

        pub fn dword(&self, name: &str) -> Option<u32> {
            self.value(name).and_then(|v| v.as_dword())
        }
    }
}

mod com {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

    /// COM initialised on the current (scan-private) thread for its lifetime.
    pub struct Apartment(bool);

    impl Apartment {
        pub fn enter() -> Apartment {
            Apartment(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
        }
    }

    impl Drop for Apartment {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// 1. Processes
// ---------------------------------------------------------------------------------------

mod processes {
    use super::*;
    use crate::process::ImagePaths;

    #[derive(Debug, PartialEq, Eq)]
    pub struct Group {
        pub name: String,
        pub pids: Vec<u32>,
        pub ram_bytes: u64,
    }

    /// Group by exe name, case-insensitively (first spelling seen wins). PID 0 (System
    /// Idle Process) is not a real process and is dropped. Sorted by name.
    pub fn group(procs: &[ProcInfo]) -> Vec<Group> {
        let mut map: HashMap<String, Group> = HashMap::new();
        for p in procs.iter().filter(|p| p.pid != 0 && !p.name.is_empty()) {
            let g = map.entry(p.name.to_lowercase()).or_insert_with(|| Group {
                name: p.name.clone(),
                pids: Vec::new(),
                ram_bytes: 0,
            });
            g.pids.push(p.pid);
            g.ram_bytes += p.working_set_bytes;
        }
        let mut out: Vec<(String, Group)> = map.into_iter().collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.into_iter()
            .map(|(_, mut g)| {
                g.pids.sort_unstable();
                g
            })
            .collect()
    }

    /// One row per exe name. `ram_bytes` is the sum of the table's working sets; `detail`
    /// is the first full image path any member yields, else just the exe name.
    pub fn scan(procs: &[ProcInfo]) -> Vec<ScanItem> {
        let mut paths = ImagePaths::new();
        group(procs)
            .into_iter()
            .map(|g| {
                let detail = g.pids.iter().find_map(|&pid| paths.get(pid)).unwrap_or_else(|| g.name.clone());
                ScanItem {
                    display: g.name.clone(),
                    name: g.name,
                    kind: Kind::Process,
                    running: true,
                    ram_bytes: g.ram_bytes,
                    pids: g.pids,
                    detail,
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------
// 2. Services
// ---------------------------------------------------------------------------------------

mod services {
    use super::*;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA};
    use windows::Win32::System::Services::{
        CloseServiceHandle, EnumServicesStatusExW, OpenSCManagerW, OpenServiceW, QueryServiceConfigW,
        ENUM_SERVICE_STATUS_PROCESSW, QUERY_SERVICE_CONFIGW, SC_ENUM_PROCESS_INFO, SC_HANDLE,
        SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE, SERVICE_QUERY_CONFIG, SERVICE_RUNNING,
        SERVICE_STATE_ALL, SERVICE_WIN32,
    };

    /// `SERVICE_USERSERVICE_INSTANCE`: set on per-user instances such as CDPUserSvc_8155c.
    const USER_SERVICE_INSTANCE: u32 = 0x80;
    /// `SERVICE_USER_SERVICE`: set on per-user templates and their instances.
    const USER_SERVICE: u32 = 0x40;

    #[derive(Debug, Clone, Default)]
    pub struct Raw {
        pub name: String,
        pub display: String,
        pub service_type: u32,
        pub running: bool,
        pub pid: u32,
        pub start_type: Option<u32>,
        pub binary: Option<String>,
    }

    struct Handle(SC_HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseServiceHandle(self.0);
            }
        }
    }

    pub fn enumerate() -> Vec<Raw> {
        let Ok(scm) = (unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE) }) else {
            return Vec::new();
        };
        let scm = Handle(scm);
        let mut out = list(&scm);
        for s in &mut out {
            if let Some((start, bin)) = query_config(&scm, &s.name) {
                s.start_type = Some(start);
                s.binary = Some(bin);
            } else if let Some(k) = reg::Key::open(reg::HKEY_LOCAL_MACHINE, &format!(r"SYSTEM\CurrentControlSet\Services\{}", s.name)) {
                // Fallback when the SCM refuses SERVICE_QUERY_CONFIG: the registry copy.
                s.start_type = k.dword("Start");
                s.binary = k.string("ImagePath");
            }
        }
        out
    }

    fn list(scm: &Handle) -> Vec<Raw> {
        let mut out = Vec::new();
        let mut resume = 0u32;
        // u64 backing keeps the struct array suitably aligned.
        let mut buf: Vec<u64> = vec![0; 64 * 1024 / 8];
        for _ in 0..64 {
            let mut needed = 0u32;
            let mut returned = 0u32;
            let bytes = unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, buf.len() * 8) };
            let r = unsafe {
                EnumServicesStatusExW(
                    scm.0,
                    SC_ENUM_PROCESS_INFO,
                    SERVICE_WIN32,
                    SERVICE_STATE_ALL,
                    Some(bytes),
                    &mut needed,
                    &mut returned,
                    Some(&mut resume),
                    PCWSTR::null(),
                )
            };
            let more = matches!(&r, Err(e) if e.code() == ERROR_MORE_DATA.to_hresult());
            if r.is_ok() || more {
                let entries = unsafe {
                    std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW, returned as usize)
                };
                for e in entries {
                    let st = &e.ServiceStatusProcess;
                    out.push(Raw {
                        name: unsafe { pwstr(e.lpServiceName) },
                        display: unsafe { pwstr(e.lpDisplayName) },
                        service_type: st.dwServiceType.0,
                        running: st.dwCurrentState == SERVICE_RUNNING,
                        pid: st.dwProcessId,
                        start_type: None,
                        binary: None,
                    });
                }
            }
            if !more {
                break;
            }
            if returned == 0 {
                let want = (needed as usize / 8 + 1).max(buf.len() * 2);
                buf.resize(want, 0);
            }
        }
        out.retain(|s| !s.name.is_empty());
        out
    }

    unsafe fn pwstr(p: windows::core::PWSTR) -> String {
        if p.is_null() {
            String::new()
        } else {
            unsafe { p.to_string() }.unwrap_or_default()
        }
    }

    fn query_config(scm: &Handle, name: &str) -> Option<(u32, String)> {
        let n = wide(name);
        let svc = Handle(unsafe { OpenServiceW(scm.0, PCWSTR(n.as_ptr()), SERVICE_QUERY_CONFIG) }.ok()?);
        let mut needed = 0u32;
        let r = unsafe { QueryServiceConfigW(svc.0, None, 0, &mut needed) };
        match r {
            Err(e) if e.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() => {}
            _ => return None,
        }
        let mut buf: Vec<u64> = vec![0; needed as usize / 8 + 1];
        let cfg = buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW;
        unsafe { QueryServiceConfigW(svc.0, Some(cfg), (buf.len() * 8) as u32, &mut needed) }.ok()?;
        let cfg = unsafe { &*cfg };
        Some((cfg.dwStartType.0, unsafe { pwstr(cfg.lpBinaryPathName) }))
    }

    pub fn start_type_name(t: Option<u32>) -> &'static str {
        match t {
            Some(0) => "boot",
            Some(1) => "system",
            Some(2) => "auto",
            Some(3) => "manual",
            Some(4) => "disabled",
            Some(_) => "unknown",
            None => "unreadable",
        }
    }

    /// For per-user service instances ("CDPUserSvc_8155c"), the template name ("CDPUserSvc").
    /// Gated on the instance type bit so ordinary names with underscores never match;
    /// the suffix must also look like the hex LUID Windows appends.
    pub fn template_name(name: &str, service_type: u32) -> Option<&str> {
        if service_type & USER_SERVICE_INSTANCE == 0 {
            return None;
        }
        let (base, suffix) = name.rsplit_once('_')?;
        let hex = !suffix.is_empty() && suffix.len() <= 16 && suffix.chars().all(|c| c.is_ascii_hexdigit());
        (hex && !base.is_empty()).then_some(base)
    }

    /// PIDs that host more than one running service. Their working set cannot be
    /// attributed to any single service, so those services report 0 RAM.
    pub fn shared_host_pids(services: &[(u32, bool)]) -> HashSet<u32> {
        let mut count: HashMap<u32, usize> = HashMap::new();
        for &(pid, running) in services {
            if running && pid != 0 {
                *count.entry(pid).or_default() += 1;
            }
        }
        count.into_iter().filter(|&(_, n)| n > 1).map(|(pid, _)| pid).collect()
    }

    pub fn is_svchost(binary: Option<&str>) -> bool {
        binary.is_some_and(|b| b.to_ascii_lowercase().contains("svchost.exe"))
    }

    pub fn to_items(raw: Vec<Raw>, ws: &HashMap<u32, u64>) -> Vec<ScanItem> {
        let pairs: Vec<(u32, bool)> = raw.iter().map(|s| (s.pid, s.running)).collect();
        let shared = shared_host_pids(&pairs);
        let mut per_pid: HashMap<u32, usize> = HashMap::new();
        for &(pid, running) in &pairs {
            if running && pid != 0 {
                *per_pid.entry(pid).or_default() += 1;
            }
        }
        raw.into_iter()
            .map(|s| {
                let pids = if s.running && s.pid != 0 { vec![s.pid] } else { Vec::new() };
                let is_shared = s.running && shared.contains(&s.pid);
                let ram_bytes = if is_shared || pids.is_empty() { 0 } else { ws.get(&s.pid).copied().unwrap_or(0) };
                let start = format!("start: {}", start_type_name(s.start_type));
                let path = s.binary.as_deref().map(|b| format!("path: {b}")).unwrap_or_default();
                let sharing = if is_shared {
                    let n = per_pid.get(&s.pid).copied().unwrap_or(0);
                    if is_svchost(s.binary.as_deref()) {
                        format!("shared svchost process ({n} services)")
                    } else {
                        format!("shared process ({n} services)")
                    }
                } else {
                    String::new()
                };
                let template = template_name(&s.name, s.service_type).map(|t| format!("template: {t}")).unwrap_or_default();
                let user_template = if s.service_type & USER_SERVICE != 0 && s.service_type & USER_SERVICE_INSTANCE == 0 {
                    "per-user service template"
                } else {
                    ""
                };
                let detail = join_detail(&[&start, &path, &sharing, &template, user_template]);
                ScanItem {
                    display: if s.display.is_empty() { s.name.clone() } else { s.display },
                    name: s.name,
                    kind: Kind::Service,
                    running: s.running,
                    ram_bytes,
                    pids,
                    detail,
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------
// 3. Startup entries
// ---------------------------------------------------------------------------------------

mod startup {
    use super::*;
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, SLGP_RAWPATH};

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const RUN_ONCE: &str = r"Software\Microsoft\Windows\CurrentVersion\RunOnce";
    const RUN_WOW: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
    const RUN_ONCE_WOW: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\RunOnce";
    const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Hive {
        User,
        Machine,
    }

    impl Hive {
        fn root(self) -> windows::Win32::System::Registry::HKEY {
            match self {
                Hive::User => reg::HKEY_CURRENT_USER,
                Hive::Machine => reg::HKEY_LOCAL_MACHINE,
            }
        }
        fn label(self) -> &'static str {
            match self {
                Hive::User => "HKCU",
                Hive::Machine => "HKLM",
            }
        }
    }

    /// Interpret a StartupApproved value. Absent or empty: enabled. Otherwise the low bit
    /// of the first byte is the "disabled" flag (0x02/0x06 enabled, 0x03/0x07 disabled).
    pub fn approved_enabled(data: Option<&[u8]>) -> bool {
        match data.and_then(|d| d.first()) {
            None => true,
            Some(b) => b & 0x01 == 0,
        }
    }

    /// StartupApproved data for one entry, from the matching hive first and the other
    /// hive second. Disabled in either one counts as disabled.
    fn enabled(hive: Hive, sub: &str, name: &str) -> bool {
        let lookup = |h: Hive| -> Option<Vec<u8>> {
            let k = reg::Key::open(h.root(), &format!(r"{APPROVED}\{sub}"))?;
            k.value(name).map(|v| v.data)
        };
        let other = if hive == Hive::User { Hive::Machine } else { Hive::User };
        approved_enabled(lookup(hive).as_deref()) && approved_enabled(lookup(other).as_deref())
    }

    pub fn scan() -> Vec<ScanItem> {
        let mut out = Vec::new();
        let sources: [(Hive, &str, Option<&str>, &str); 8] = [
            (Hive::User, RUN, Some("Run"), "Run"),
            (Hive::User, RUN_ONCE, None, "RunOnce"),
            (Hive::User, RUN_WOW, Some("Run32"), "Run (32-bit)"),
            (Hive::User, RUN_ONCE_WOW, None, "RunOnce (32-bit)"),
            (Hive::Machine, RUN, Some("Run"), "Run"),
            (Hive::Machine, RUN_ONCE, None, "RunOnce"),
            (Hive::Machine, RUN_WOW, Some("Run32"), "Run (32-bit)"),
            (Hive::Machine, RUN_ONCE_WOW, None, "RunOnce (32-bit)"),
        ];
        for (hive, path, approved, label) in sources {
            let Some(key) = reg::Key::open(hive.root(), path) else { continue };
            for (name, value) in key.values() {
                let Some(cmd) = value.as_string() else { continue };
                // RunOnce entries are not covered by StartupApproved: they always run once.
                let running = approved.is_none_or(|sub| enabled(hive, sub, &name));
                let source = format!("source: {} {}", hive.label(), label);
                out.push(ScanItem {
                    display: name.clone(),
                    name,
                    kind: Kind::Startup,
                    running,
                    ram_bytes: 0,
                    pids: Vec::new(),
                    detail: join_detail(&[&cmd, &source]),
                });
            }
        }

        let folders = [
            (Hive::User, std::env::var_os("APPDATA"), "user Startup folder"),
            (Hive::Machine, std::env::var_os("ProgramData"), "all-users Startup folder"),
        ];
        for (hive, base, label) in folders {
            let Some(base) = base else { continue };
            let dir = Path::new(&base).join(r"Microsoft\Windows\Start Menu\Programs\Startup");
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(true) {
                    continue;
                }
                let file = e.file_name().to_string_lossy().into_owned();
                if file.eq_ignore_ascii_case("desktop.ini") {
                    continue;
                }
                let path = e.path();
                let cmd = if file.to_ascii_lowercase().ends_with(".lnk") {
                    resolve_lnk(&path).unwrap_or_else(|| path.display().to_string())
                } else {
                    path.display().to_string()
                };
                let display = Path::new(&file)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| file.clone());
                let running = enabled(hive, "StartupFolder", &file);
                let source = format!("source: {label}");
                out.push(ScanItem {
                    name: file,
                    display,
                    kind: Kind::Startup,
                    running,
                    ram_bytes: 0,
                    pids: Vec::new(),
                    detail: join_detail(&[&cmd, &source]),
                });
            }
        }
        out
    }

    /// Target and arguments of a shortcut, without resolving or touching it.
    fn resolve_lnk(path: &Path) -> Option<String> {
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
            let file: IPersistFile = link.cast().ok()?;
            let p = wide(&path.to_string_lossy());
            file.Load(PCWSTR(p.as_ptr()), STGM_READ).ok()?;
            let mut target = [0u16; 1024];
            link.GetPath(&mut target, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32).ok()?;
            let mut args = [0u16; 2048];
            let _ = link.GetArguments(&mut args);
            let target = from_wide(&target);
            let args = from_wide(&args);
            if target.is_empty() {
                return None;
            }
            let quoted = if target.contains(' ') { format!("\"{target}\"") } else { target };
            Some(if args.is_empty() { quoted } else { format!("{quoted} {args}") })
        }
    }
}

// ---------------------------------------------------------------------------------------
// 4. Scheduled tasks (Task Scheduler 2.0 COM API; never schtasks.exe)
// ---------------------------------------------------------------------------------------

mod tasks {
    use super::*;
    use windows::core::BSTR;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::System::TaskScheduler::{ITaskFolder, ITaskService, TaskScheduler, TASK_ENUM_HIDDEN};
    use windows::Win32::System::Variant::VARIANT;

    const MAX_DEPTH: usize = 32;

    pub fn scan() -> Vec<ScanItem> {
        let mut out = Vec::new();
        unsafe {
            let Ok(svc) = CoCreateInstance::<_, ITaskService>(&TaskScheduler, None, CLSCTX_INPROC_SERVER) else {
                return out;
            };
            let empty = VARIANT::default();
            if svc.Connect(&empty, &empty, &empty, &empty).is_err() {
                return out;
            }
            let Ok(root) = svc.GetFolder(&BSTR::from("\\")) else { return out };
            walk(&root, 0, &mut out);
        }
        out
    }

    unsafe fn walk(folder: &ITaskFolder, depth: usize, out: &mut Vec<ScanItem>) {
        if depth > MAX_DEPTH {
            return;
        }
        unsafe {
            let path = folder.Path().map(|b| b.to_string()).unwrap_or_default();
            if let Ok(tasks) = folder.GetTasks(TASK_ENUM_HIDDEN.0) {
                let n = tasks.Count().unwrap_or(0);
                for i in 1..=n {
                    let Ok(t) = tasks.get_Item(&VARIANT::from(i)) else { continue };
                    let Ok(name) = t.Name() else { continue };
                    let name = name.to_string();
                    let enabled = t.Enabled().map(|b| b.as_bool()).unwrap_or(false);
                    out.push(ScanItem {
                        display: name.clone(),
                        name,
                        kind: Kind::Task,
                        running: enabled,
                        ram_bytes: 0,
                        pids: Vec::new(),
                        detail: path.clone(),
                    });
                }
            }
            if let Ok(subs) = folder.GetFolders(0) {
                let n = subs.Count().unwrap_or(0);
                for i in 1..=n {
                    if let Ok(sub) = subs.get_Item(&VARIANT::from(i)) {
                        walk(&sub, depth + 1, out);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// 5. Installed apps (Uninstall keys)
// ---------------------------------------------------------------------------------------

mod apps {
    use super::*;

    const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";
    const UNINSTALL_WOW: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";

    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct Entry {
        pub display_name: Option<String>,
        pub system_component: Option<u32>,
        pub parent_key_name: Option<String>,
        pub release_type: Option<String>,
        pub publisher: String,
        pub version: String,
        pub install_location: String,
    }

    /// Drop entries the user would not recognise as an app: no DisplayName,
    /// SystemComponent=1, updates (ParentKeyName set, or ReleaseType Update/Hotfix/
    /// Security Update), and duplicates by display name (case-insensitive, first wins).
    pub fn filter(entries: Vec<Entry>) -> Vec<Entry> {
        let mut seen = HashSet::new();
        entries
            .into_iter()
            .filter(|e| {
                let Some(name) = e.display_name.as_deref().map(str::trim) else { return false };
                if name.is_empty() || e.system_component == Some(1) {
                    return false;
                }
                if e.parent_key_name.as_deref().is_some_and(|p| !p.trim().is_empty()) {
                    return false;
                }
                if let Some(rt) = e.release_type.as_deref() {
                    let rt = rt.trim().to_ascii_lowercase();
                    if rt == "update" || rt == "hotfix" || rt == "security update" {
                        return false;
                    }
                }
                seen.insert(name.to_lowercase())
            })
            .collect()
    }

    fn read(root: windows::Win32::System::Registry::HKEY, path: &str) -> Vec<Entry> {
        let Some(key) = reg::Key::open(root, path) else { return Vec::new() };
        key.subkeys()
            .into_iter()
            .filter_map(|sub| {
                let k = key.open_sub(&sub)?;
                Some(Entry {
                    display_name: k.string("DisplayName"),
                    system_component: k.dword("SystemComponent"),
                    parent_key_name: k.string("ParentKeyName"),
                    release_type: k.string("ReleaseType"),
                    publisher: k.string("Publisher").unwrap_or_default().trim().to_string(),
                    version: k.string("DisplayVersion").unwrap_or_default().trim().to_string(),
                    install_location: k.string("InstallLocation").unwrap_or_default().trim().to_string(),
                })
            })
            .collect()
    }

    pub fn scan() -> Vec<ScanItem> {
        let mut all = read(reg::HKEY_LOCAL_MACHINE, UNINSTALL);
        all.extend(read(reg::HKEY_LOCAL_MACHINE, UNINSTALL_WOW));
        all.extend(read(reg::HKEY_CURRENT_USER, UNINSTALL));
        all.extend(read(reg::HKEY_CURRENT_USER, UNINSTALL_WOW));
        filter(all)
            .into_iter()
            .map(|e| {
                let name = e.display_name.unwrap_or_default().trim().to_string();
                ScanItem {
                    display: name.clone(),
                    name,
                    kind: Kind::App,
                    running: false,
                    ram_bytes: 0,
                    pids: Vec::new(),
                    detail: format!("{} | {} | {}", e.publisher, e.version, e.install_location),
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, name: &str, ws: u64) -> ProcInfo {
        ProcInfo { pid, parent: 0, name: name.into(), working_set_bytes: ws }
    }

    #[test]
    fn startup_approved_bytes() {
        use startup::approved_enabled;
        assert!(approved_enabled(None));
        assert!(approved_enabled(Some(&[])));
        assert!(approved_enabled(Some(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])));
        assert!(approved_enabled(Some(&[0x06, 0, 0, 0])));
        assert!(!approved_enabled(Some(&[0x03, 0x11, 0x22, 0x33, 0, 0, 0, 0, 0, 0, 0, 0])));
        assert!(!approved_enabled(Some(&[0x07])));
    }

    fn entry(name: Option<&str>) -> apps::Entry {
        apps::Entry { display_name: name.map(Into::into), ..Default::default() }
    }

    #[test]
    fn uninstall_filtering() {
        let mut sys = entry(Some("Hidden runtime"));
        sys.system_component = Some(1);
        let mut visible_sc0 = entry(Some("Visible"));
        visible_sc0.system_component = Some(0);
        let mut update = entry(Some("Update for Office (KB123)"));
        update.parent_key_name = Some("Office16".into());
        let mut hotfix = entry(Some("Hotfix thing"));
        hotfix.release_type = Some("Hotfix".into());
        let mut dup = entry(Some("  chrome  "));
        dup.version = "2".into();
        let mut first = entry(Some("Chrome"));
        first.version = "1".into();
        let out = apps::filter(vec![
            entry(None),
            entry(Some("   ")),
            sys,
            visible_sc0,
            update,
            hotfix,
            first,
            dup,
            entry(Some("Discord")),
        ]);
        let names: Vec<_> = out.iter().map(|e| e.display_name.clone().unwrap()).collect();
        assert_eq!(names, vec!["Visible", "Chrome", "Discord"]);
        assert_eq!(out[1].version, "1", "first duplicate wins");
    }

    #[test]
    fn service_template_names() {
        use services::template_name;
        assert_eq!(template_name("CDPUserSvc_8155c", 0xE0), Some("CDPUserSvc"));
        assert_eq!(template_name("OneSyncSvc_2c8a3", 0xF0), Some("OneSyncSvc"));
        assert_eq!(template_name("Some_Name_abc12", 0xD0), Some("Some_Name"));
        // Template itself, or not an instance: no template.
        assert_eq!(template_name("CDPUserSvc", 0x60), None);
        assert_eq!(template_name("CDPUserSvc_8155c", 0x20), None);
        // Instance bit but suffix not hex, or nothing around the underscore.
        assert_eq!(template_name("Foo_xyz", 0xE0), None);
        assert_eq!(template_name("_8155c", 0xE0), None);
        assert_eq!(template_name("Foo_", 0xE0), None);
        assert_eq!(template_name("NoUnderscore", 0xE0), None);
    }

    #[test]
    fn svchost_shared_hosts() {
        use services::{shared_host_pids, to_items, Raw};
        let shared = shared_host_pids(&[(100, true), (100, true), (200, true), (0, false), (0, false), (300, false), (300, true)]);
        assert_eq!(shared, HashSet::from([100]));

        let svc = |name: &str, pid: u32, running: bool, bin: &str| Raw {
            name: name.into(),
            display: format!("{name} display"),
            service_type: 0x20,
            running,
            pid,
            start_type: Some(2),
            binary: Some(bin.into()),
        };
        let ws = HashMap::from([(100u32, 50_000_000u64), (200, 7_000_000), (300, 1_000)]);
        let items = to_items(
            vec![
                svc("A", 100, true, r"C:\Windows\system32\svchost.exe -k netsvcs -p"),
                svc("B", 100, true, r"C:\Windows\system32\svchost.exe -k netsvcs -p"),
                svc("C", 200, true, r"C:\Windows\System32\svchost.exe -k LocalServiceNoNetwork -p"),
                svc("D", 0, false, r"C:\Program Files\D\d.exe"),
            ],
            &ws,
        );
        let by: HashMap<_, _> = items.iter().map(|i| (i.name.as_str(), i)).collect();
        assert_eq!(by["A"].ram_bytes, 0);
        assert!(by["A"].detail.contains("shared svchost process"));
        assert_eq!(by["B"].ram_bytes, 0);
        // An svchost hosting a single service owns its working set: no double count.
        assert_eq!(by["C"].ram_bytes, 7_000_000);
        assert!(!by["C"].detail.contains("shared"));
        assert_eq!(by["C"].pids, vec![200]);
        assert!(!by["D"].running);
        assert!(by["D"].pids.is_empty());
        assert_eq!(by["D"].ram_bytes, 0);
        assert!(by["D"].detail.contains("start: auto"));
        assert!(by["D"].detail.contains(r"path: C:\Program Files\D\d.exe"));
        // Sum of service RAM never exceeds the sum of distinct host working sets.
        let total: u64 = items.iter().map(|i| i.ram_bytes).sum();
        assert!(total <= 50_000_000 + 7_000_000 + 1_000);
    }

    #[test]
    fn service_instance_detail_has_template() {
        let raw = services::Raw {
            name: "CDPUserSvc_8155c".into(),
            display: "Connected Devices Platform User Service_8155c".into(),
            service_type: 0xE0,
            running: true,
            pid: 42,
            start_type: Some(2),
            binary: Some(r"C:\Windows\system32\svchost.exe -k UnistackSvcGroup".into()),
        };
        let items = services::to_items(vec![raw], &HashMap::from([(42, 10)]));
        assert_eq!(items[0].name, "CDPUserSvc_8155c", "name is never stripped");
        assert!(items[0].detail.contains("template: CDPUserSvc"));
        assert_eq!(items[0].ram_bytes, 10);
    }

    #[test]
    fn process_grouping() {
        let groups = processes::group(&[
            proc(0, "[System Process]", 0),
            proc(4, "System", 100),
            proc(10, "chrome.exe", 1000),
            proc(12, "Chrome.exe", 500),
            proc(11, "chrome.exe", 250),
            proc(20, "discord.exe", 300),
            proc(30, "", 5),
        ]);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].name, "chrome.exe");
        assert_eq!(groups[0].pids, vec![10, 11, 12]);
        assert_eq!(groups[0].ram_bytes, 1750);
        assert_eq!(groups[1].name, "discord.exe");
        assert_eq!(groups[2].name, "System");
        assert!(groups.iter().all(|g| !g.pids.contains(&0)));
    }

    #[test]
    fn reg_string_decoding() {
        let mut bytes: Vec<u8> = "C:\\a b.exe".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        bytes.extend([0, 0, 0]); // terminator plus a stray odd byte
        assert_eq!(reg_bytes_to_string(&bytes), "C:\\a b.exe");
        assert_eq!(join_detail(&["a", "", "b"]), "a | b");
    }

    /// Per-source timings and what could not be read. Run with
    /// `cargo test -p holdmyram-core scan_report -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn scan_report() {
        let time = |label: &str, t: std::time::Instant| eprintln!("{label}: {:?}", t.elapsed());
        let t = std::time::Instant::now();
        let procs = list_processes();
        time("list_processes", t);
        let t = std::time::Instant::now();
        let p = processes::scan(&procs);
        time("process groups + paths", t);
        let t = std::time::Instant::now();
        let raw = services::enumerate();
        time("services", t);
        let ws: HashMap<u32, u64> = procs.iter().map(|p| (p.pid, p.working_set_bytes)).collect();
        let _com = com::Apartment::enter();
        let t = std::time::Instant::now();
        let st = startup::scan();
        time("startup", t);
        let t = std::time::Instant::now();
        let tk = tasks::scan();
        time("tasks", t);
        let t = std::time::Instant::now();
        let ap = apps::scan();
        time("apps", t);

        // BEFORE = the old per-process method (OpenProcess + GetProcessMemoryInfo /
        // QueryFullProcessImageNameW), measured now on the same PIDs for comparison.
        // AFTER = the system process table + SystemProcessIdInformation.
        let real: Vec<&ProcInfo> = procs.iter().filter(|p| p.pid != 0).collect();
        let old_ws: HashMap<u32, u64> = procs.iter().map(|p| (p.pid, crate::process::working_set(p.pid))).collect();
        let before_ws0 = real.iter().filter(|p| old_ws[&p.pid] == 0).count();
        let after_ws0: Vec<&&ProcInfo> = real.iter().filter(|p| p.working_set_bytes == 0).collect();
        eprintln!("processes: {} pids (excluding idle)", real.len());
        eprintln!("  working set 0: BEFORE {before_ws0}, AFTER {}", after_ws0.len());
        for q in &after_ws0 {
            eprintln!("    still 0: pid {} {}", q.pid, q.name);
        }
        let groups = processes::group(&procs);
        let before_np = groups.iter().filter(|g| !g.pids.iter().any(|&pid| crate::process::query_full_image_name(pid).is_some())).count();
        // A full path has a backslash; the exe-name fallback does not.
        let no_path: Vec<&ScanItem> = p.iter().filter(|i| !i.detail.contains('\\')).collect();
        eprintln!("process groups: {}", p.len());
        eprintln!("  no full path: BEFORE {before_np}, AFTER {}", no_path.len());
        for i in &no_path {
            eprintln!("    still no path: {} (pids {:?})", i.name, i.pids);
        }
        for i in p.iter().filter(|i| !i.name.to_ascii_lowercase().ends_with(".exe") || i.name.eq_ignore_ascii_case("MsMpEng.exe") || i.name.eq_ignore_ascii_case("csrss.exe")) {
            eprintln!("    sample: {} -> {}", i.name, i.detail);
        }
        let not_mapped = p.iter().filter(|i| i.detail.starts_with(r"\Device\")).count();
        eprintln!("  paths left as NT device paths (no drive letter): {not_mapped}");
        let no_cfg = raw.iter().filter(|s| s.start_type.is_none()).count();
        let instances = raw.iter().filter(|s| s.service_type & 0x80 != 0).count();
        let sv_before = services::to_items(raw.clone(), &old_ws);
        let zero = |v: &[ScanItem]| v.iter().filter(|s| s.running && s.ram_bytes == 0 && !s.detail.contains("shared")).count();
        let sv = services::to_items(raw, &ws);
        let running_zero = zero(&sv);
        eprintln!("  running unshared services with 0 RAM: BEFORE {}, AFTER {running_zero}", zero(&sv_before));
        for s in sv.iter().filter(|s| s.running && s.ram_bytes == 0 && !s.detail.contains("shared")) {
            eprintln!("    still 0: {} pids {:?}", s.name, s.pids);
        }
        let t = std::time::Instant::now();
        let all = scan_all();
        eprintln!("scan_all: {} items in {:?}", all.len(), t.elapsed());
        eprintln!(
            "services: {} ({} running, {} shared-host, {} instances, {} config unreadable, {} running unshared with 0 RAM)",
            sv.len(),
            sv.iter().filter(|s| s.running).count(),
            sv.iter().filter(|s| s.detail.contains("shared")).count(),
            instances,
            no_cfg,
            running_zero
        );
        for s in sv.iter().filter(|s| s.detail.contains("template:")).take(3) {
            eprintln!("  e.g. {} -> {}", s.name, s.detail);
        }
        eprintln!("startup: {} ({} disabled)", st.len(), st.iter().filter(|s| !s.running).count());
        for s in &st {
            eprintln!("  {} [{}] {}", s.name, if s.running { "on" } else { "off" }, s.detail);
        }
        eprintln!(
            "tasks: {} ({} disabled, {} under \\Microsoft\\Windows\\)",
            tk.len(),
            tk.iter().filter(|s| !s.running).count(),
            tk.iter().filter(|s| s.detail.starts_with("\\Microsoft\\Windows\\")).count()
        );
        eprintln!("apps: {}", ap.len());
    }

    /// Runs the real scan on this machine. Read-only.
    #[test]
    fn smoke_scan_all() {
        let t = std::time::Instant::now();
        let items = scan_all();
        let elapsed = t.elapsed();
        let mut counts: HashMap<Kind, usize> = HashMap::new();
        for i in &items {
            *counts.entry(i.kind).or_default() += 1;
        }
        eprintln!("scan_all: {} items in {:?}: {:?}", items.len(), elapsed, counts);
        for k in [Kind::Process, Kind::Service, Kind::Startup, Kind::Task, Kind::App] {
            assert!(counts.get(&k).copied().unwrap_or(0) > 0, "no items of kind {k:?}");
        }
        assert!(elapsed.as_secs_f64() < 5.0, "scan took {elapsed:?}");
        assert!(items.iter().all(|i| !i.name.is_empty()));
    }
}
