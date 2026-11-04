//! Hard-coded safety rules for the elevated helper. ONE place: the helper, the app-side
//! client and the change log all use these, and the helper re-checks every request
//! against them no matter what the app already checked.
//!
//! Matching is deliberately conservative: case-insensitive, and a service name with a
//! `_<hex>` suffix is also checked as its per-user template (`Foo_1a2b3` -> `Foo`) whether
//! or not Windows marks it as a per-user instance. A false "blocked" is harmless; a false
//! "allowed" is not.

/// Services the helper refuses to stop, start or reconfigure, even if asked.
pub const NEVER_TOUCH_SERVICES: &[&str] = &[
    // Core Windows: RPC/COM/session plumbing, WMI, networking basics, power, audio,
    // logging, scheduling, firewall, crypto, accounts, profiles, PnP, app model, policy,
    // file sharing, domain, servicing.
    "RpcSs",
    "RpcEptMapper",
    "DcomLaunch",
    "LSM",
    "CoreMessagingRegistrar",
    "Winmgmt",
    "NlaSvc",
    "Wcmsvc",
    "nsi",
    "Dhcp",
    "Dnscache",
    "BrokerInfrastructure",
    "Power",
    "AudioSrv",
    "AudioEndpointBuilder",
    "EventLog",
    "EventSystem",
    "Schedule",
    "BFE",
    "mpssvc",
    "CryptSvc",
    "SamSs",
    "ProfSvc",
    "UserManager",
    "PlugPlay",
    "DeviceInstall",
    "StateRepository",
    "gpsvc",
    "LanmanWorkstation",
    "LanmanServer",
    "Netlogon",
    "TrustedInstaller",
    // Security.
    "WinDefend",
    "MDCoreSvc",
    "WdNisSvc",
    "SecurityHealthService",
    "Sense",
    "wscsvc",
    // Remote desktop.
    "TermService",
    // Anti-cheat.
    "vgc",
    "vgtray",
    "BEService",
    // BattlEye's kernel driver service. Drivers are refused by type anyway; named here so
    // the block does not depend on the type bits being readable.
    "BEDaisy",
    "EasyAntiCheat",
    "EasyAntiCheat_EOS",
    "EAAntiCheat.GameService",
    "FACEITService",
    "FACEITClient",
    // Hardware vendor services this machine depends on.
    "NvContainerLocalSystem",
    "AsusOptimization",
    "AsusPTPService",
    "AsusAppService",
    "ASUSSystemAnalysis",
    "ASUSSystemDiagnosis",
];

/// Task folders (and everything below them) the helper refuses to enable or disable.
/// Spellings checked against this machine's Task Scheduler on 2026-11-04: Windows
/// Defender, SoftwareProtectionPlatform, Servicing, WindowsUpdate and Time
/// Synchronization exist and are visible to the user; TaskScheduler and
/// UpdateOrchestrator are not visible without admin and are listed by their documented
/// names. Matching is case-insensitive, by path prefix.
pub const NEVER_TOUCH_TASK_FOLDERS: &[&str] = &[
    r"\Microsoft\Windows\Windows Defender\",
    r"\Microsoft\Windows\TaskScheduler\",
    r"\Microsoft\Windows\SoftwareProtectionPlatform\",
    r"\Microsoft\Windows\Servicing\",
    r"\Microsoft\Windows\WindowsUpdate\",
    r"\Microsoft\Windows\UpdateOrchestrator\",
    r"\Microsoft\Windows\Time Synchronization\",
];

/// Service type bits for drivers: kernel (0x1), file system (0x2), adapter (0x4) and
/// recognizer (0x8). Any of them means "driver": never touched.
pub const SERVICE_DRIVER_MASK: u32 = 0x0000_000F;

/// SCM limit on service key names.
pub const MAX_SERVICE_NAME: usize = 256;
/// Generous limit for a task path; real ones are well under 300 characters.
pub const MAX_TASK_PATH: usize = 1024;

/// Characters a service name may never contain (path separators would make the name
/// mean something other than a plain SCM key).
const SERVICE_FORBIDDEN: &[char] = &['\\', '/'];
/// Characters a task path component may never contain (not valid in task names, and
/// some have special meaning to the file system the task store lives on).
const TASK_FORBIDDEN: &[char] = &['/', '<', '>', ':', '"', '|', '?', '*'];

fn has_control(s: &str) -> bool {
    s.chars().any(|c| c.is_control())
}

/// A plain SCM service key name: 1..=256 UTF-16 units, no path separators, no control
/// characters, no leading or trailing whitespace.
pub fn validate_service_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("service name is empty".into());
    }
    if name.encode_utf16().count() > MAX_SERVICE_NAME {
        return Err(format!("service name is longer than {MAX_SERVICE_NAME} characters"));
    }
    if has_control(name) {
        return Err("service name contains a control character".into());
    }
    if name.contains(SERVICE_FORBIDDEN) {
        return Err("service name contains a path separator".into());
    }
    if name.trim() != name {
        return Err("service name has leading or trailing whitespace".into());
    }
    Ok(())
}

/// `Name_8155c` -> `Name`: the per-user template of a service instance. Unlike scan.rs
/// this ignores the service type bits and accepts 1..=16 hex digits, so it can only make
/// matching broader (it is used for blocking only).
pub fn service_template(name: &str) -> Option<&str> {
    let (base, suffix) = name.rsplit_once('_')?;
    let hex = (1..=16).contains(&suffix.len()) && suffix.chars().all(|c| c.is_ascii_hexdigit());
    (hex && !base.is_empty()).then_some(base)
}

/// The NEVER_TOUCH_SERVICES entry this name matches (exactly or through its template),
/// case-insensitively.
pub fn never_touch_service(name: &str) -> Option<&'static str> {
    let name = name.trim();
    let hit = |n: &str| NEVER_TOUCH_SERVICES.iter().copied().find(|b| b.eq_ignore_ascii_case(n) || b.to_lowercase() == n.to_lowercase());
    hit(name).or_else(|| service_template(name).and_then(hit))
}

/// Kernel, file-system, adapter or recognizer driver.
pub fn is_driver_type(service_type: u32) -> bool {
    service_type & SERVICE_DRIVER_MASK != 0
}

/// A full task path: `\Folder\...\Name`. Rooted at `\`, at least one component, no empty
/// components (so no `\\` and no trailing `\`), no `.` or `..`, no component with leading
/// or trailing whitespace or a trailing dot (the file system would strip those), no `/`,
/// no control characters, no characters that are invalid in task names.
pub fn validate_task_path(path: &str) -> Result<(), String> {
    if path.encode_utf16().count() > MAX_TASK_PATH {
        return Err(format!("task path is longer than {MAX_TASK_PATH} characters"));
    }
    let Some(rest) = path.strip_prefix('\\') else {
        return Err("task path must start with \\".into());
    };
    if rest.is_empty() {
        return Err("task path names no task".into());
    }
    if has_control(path) {
        return Err("task path contains a control character".into());
    }
    if path.contains(TASK_FORBIDDEN) {
        return Err("task path contains a character that is not allowed".into());
    }
    for part in rest.split('\\') {
        if part.is_empty() {
            return Err("task path has an empty component".into());
        }
        if part == "." || part == ".." {
            return Err("task path may not contain . or ..".into());
        }
        if part.trim() != part {
            return Err("task path component has leading or trailing whitespace".into());
        }
        if part.ends_with('.') {
            return Err("task path component ends with a dot".into());
        }
    }
    Ok(())
}

/// The NEVER_TOUCH_TASK_FOLDERS entry this task path is under, case-insensitively.
pub fn never_touch_task(path: &str) -> Option<&'static str> {
    let p = path.to_lowercase();
    NEVER_TOUCH_TASK_FOLDERS.iter().copied().find(|f| p.starts_with(&f.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_is_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for s in NEVER_TOUCH_SERVICES {
            validate_service_name(s).unwrap_or_else(|e| panic!("{s}: {e}"));
            assert!(seen.insert(s.to_lowercase()), "duplicate {s}");
        }
        for f in NEVER_TOUCH_TASK_FOLDERS {
            assert!(f.starts_with(r"\Microsoft\Windows\") && f.ends_with('\\'), "{f}");
            // A folder path plus a task name must be a valid task path.
            validate_task_path(&format!("{f}X")).unwrap();
        }
        // Spot-check the lists contain what the design requires.
        for s in ["RpcSs", "WinDefend", "vgc", "EAAntiCheat.GameService", "ASUSSystemDiagnosis", "Schedule", "TrustedInstaller", "BEDaisy", "FACEITClient"] {
            assert!(NEVER_TOUCH_SERVICES.contains(&s), "{s}");
        }
    }

    #[test]
    fn service_matching_is_case_insensitive() {
        assert_eq!(never_touch_service("RpcSs"), Some("RpcSs"));
        assert_eq!(never_touch_service("rpcss"), Some("RpcSs"));
        assert_eq!(never_touch_service("RPCSS"), Some("RpcSs"));
        assert_eq!(never_touch_service("asusoptimization"), Some("AsusOptimization"));
        assert_eq!(never_touch_service("ASUSOptimization"), Some("AsusOptimization"));
        assert_eq!(never_touch_service("eaanticheat.gameservice"), Some("EAAntiCheat.GameService"));
        assert_eq!(never_touch_service("easyanticheat_eos"), Some("EasyAntiCheat_EOS"));
        assert_eq!(never_touch_service("wdnissvc"), Some("WdNisSvc"));
        assert_eq!(never_touch_service("bedaisy"), Some("BEDaisy"));
        assert_eq!(never_touch_service("BEDAISY"), Some("BEDaisy"));
        assert_eq!(never_touch_service("FaceitClient"), Some("FACEITClient"));
    }

    #[test]
    fn service_matching_strips_instance_suffixes() {
        assert_eq!(service_template("CDPUserSvc_8155c"), Some("CDPUserSvc"));
        assert_eq!(service_template("X_ABCDEF0123456789"), Some("X"));
        assert_eq!(service_template("X_ABCDEF01234567890"), None, "17 hex digits");
        assert_eq!(service_template("X_"), None);
        assert_eq!(service_template("_abc"), None);
        assert_eq!(service_template("EasyAntiCheat_EOS"), None, "EOS is not hex");
        // Per-user-instance forms of blocked services are blocked.
        assert_eq!(never_touch_service("AudioSrv_8155c"), Some("AudioSrv"));
        assert_eq!(never_touch_service("audiosrv_8155C"), Some("AudioSrv"));
        assert_eq!(never_touch_service("Power_1"), Some("Power"));
        assert_eq!(never_touch_service("EasyAntiCheat_EOS_2c8a3"), Some("EasyAntiCheat_EOS"));
        // Ordinary names are not blocked.
        for ok in ["SysMain", "CDPUserSvc_8155c", "DiagTrack", "Spooler", "WSearch", "CodexSandboxService.OpenAI.Codex", "CoworkVMService", "RpcSsX", "XRpcSs", "Power_zz", "PowerShell"] {
            assert_eq!(never_touch_service(ok), None, "{ok}");
        }
    }

    #[test]
    fn service_name_validation() {
        for ok in ["SysMain", "CDPUserSvc_8155c", "EAAntiCheat.GameService", "Some Name With Spaces", "a"] {
            validate_service_name(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        let long = "a".repeat(MAX_SERVICE_NAME + 1);
        for bad in ["", " SysMain", "SysMain ", r"..\RpcSs", "a/b", r"a\b", "a\0b", "a\nb", "\t", long.as_str()] {
            assert!(validate_service_name(bad).is_err(), "{bad:?}");
        }
        validate_service_name(&"a".repeat(MAX_SERVICE_NAME)).unwrap();
    }

    #[test]
    fn driver_types() {
        for t in [0x1, 0x2, 0x4, 0x8, 0x3, 0x11] {
            assert!(is_driver_type(t), "{t:#x}");
        }
        for t in [0x10, 0x20, 0x50, 0x60, 0xE0, 0x110] {
            assert!(!is_driver_type(t), "{t:#x}");
        }
    }

    #[test]
    fn task_path_validation() {
        for ok in [
            r"\MyTask",
            r"\Microsoft\Windows\Defrag\ScheduledDefrag",
            r"\GoogleUpdateTaskMachineUA{1234-ABCD}",
            r"\Folder With Space\Task Name",
            r"\NvTmRep_CrashReport1_{B2FE1952-0186-46C3-BAEC-A80AA35AC5B8}",
        ] {
            validate_task_path(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        let long = format!(r"\{}", "a".repeat(MAX_TASK_PATH));
        for bad in [
            "",
            r"\",
            "MyTask",
            r"Microsoft\Windows\X",
            r"\\server\x",
            r"\a\\b",
            r"\a\",
            r"\a\..\b",
            r"\..",
            r"\.\x",
            r"\Microsoft\Windows\Windows Defender\..\..\x",
            r"\Microsoft\Windows\Windows Defender \x",
            r"\Microsoft\Windows\Windows Defender.\x",
            r"\ x",
            r"\a/b",
            r"\a:b",
            r"\a*",
            r"\a?",
            "\\a\0b",
            "\\a\nb",
            long.as_str(),
        ] {
            assert!(validate_task_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn task_folder_matching() {
        assert_eq!(
            never_touch_task(r"\Microsoft\Windows\Windows Defender\Windows Defender Scheduled Scan"),
            Some(r"\Microsoft\Windows\Windows Defender\")
        );
        assert!(never_touch_task(r"\microsoft\windows\windows defender\x").is_some());
        assert!(never_touch_task(r"\MICROSOFT\WINDOWS\UPDATEORCHESTRATOR\Schedule Scan").is_some());
        assert!(never_touch_task(r"\Microsoft\Windows\WindowsUpdate\Scheduled Start").is_some());
        assert!(never_touch_task(r"\Microsoft\Windows\Time Synchronization\SynchronizeTime").is_some());
        assert!(never_touch_task(r"\Microsoft\Windows\TaskScheduler\Maintenance Configurator").is_some());
        assert!(never_touch_task(r"\Microsoft\Windows\Servicing\StartComponentCleanup").is_some());
        assert!(never_touch_task(r"\Microsoft\Windows\SoftwareProtectionPlatform\SvcRestartTask").is_some());
        // Deeper subfolders are covered too.
        assert!(never_touch_task(r"\Microsoft\Windows\UpdateOrchestrator\Sub\Task").is_some());
        // Siblings with a shared prefix and unrelated tasks are not.
        assert_eq!(never_touch_task(r"\Microsoft\Windows\Windows Defender Extra\x"), None);
        assert_eq!(never_touch_task(r"\Microsoft\Windows\WindowsUpdateX\x"), None);
        assert_eq!(never_touch_task(r"\Microsoft\Windows\Defrag\ScheduledDefrag"), None);
        assert_eq!(never_touch_task(r"\GoogleUpdateTaskMachineUA"), None);
        // A TASK named like the folder, one level up, is not inside the folder.
        assert_eq!(never_touch_task(r"\Microsoft\Windows\Windows Defender"), None);
    }
}
