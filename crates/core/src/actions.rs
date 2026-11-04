//! Win32 primitives that act on one process: close (WM_CLOSE then force), suspend,
//! resume, terminate, relaunch. Each works on a [`ProcHandle`] that is held open for the
//! whole action, so the PID cannot be recycled under us mid-action.
//!
//! Nothing here decides *what* to touch; that is `plan` + `toggle`. Every call returns a
//! result instead of panicking.

use crate::journal::{Entry, EntryState};
use crate::types::Method;
use std::collections::HashSet;
use std::ffi::c_void;
use std::path::Path;
use std::time::{Duration, Instant};
use windows::core::{s, w, BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject, PROCESS_ACCESS_RIGHTS, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SUSPEND_RESUME, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindow, GetWindowThreadProcessId, IsWindowVisible,
    PostMessageW, GW_OWNER, WM_CLOSE,
};

/// Default wait between WM_CLOSE and TerminateProcess.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);

/// Exit code used when we force a process down, so it is recognisable in logs.
const FORCED_EXIT_CODE: u32 = 0x484D_5221; // "HMR!"

/// Directories whose executables are never touched, whatever the selection says.
/// Compared case-insensitively as path prefixes. `%SystemRoot%` is resolved at runtime.
const PROTECTED_DIRS: &[&str] = &[
    r"C:\Windows\",
    r"C:\Program Files\Windows Defender\",
    r"C:\Program Files (x86)\Windows Defender\",
    r"C:\ProgramData\Microsoft\Windows Defender\",
    r"C:\Program Files\Windows Defender Advanced Threat Protection\",
];

// ---------------------------------------------------------------------------------------
// Process handle and identity
// ---------------------------------------------------------------------------------------

/// Who a process is, beyond its PID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub exe: String,
    pub created: Option<u64>,
}

/// An open process handle, closed on drop.
pub struct ProcHandle {
    pub pid: u32,
    handle: HANDLE,
}

// HANDLE is a plain kernel handle; using it from another thread is fine.
unsafe impl Send for ProcHandle {}

impl Drop for ProcHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

impl ProcHandle {
    pub fn open(pid: u32, rights: PROCESS_ACCESS_RIGHTS) -> Result<Self, String> {
        if pid == 0 || pid == 4 {
            return Err("system process".into());
        }
        unsafe {
            OpenProcess(rights, false, pid)
                .map(|handle| Self { pid, handle })
                .map_err(|e| format!("cannot open process: {}", e.message()))
        }
    }

    /// Rights needed to carry out `method` (and to verify identity first).
    pub fn rights_for(method: Method) -> PROCESS_ACCESS_RIGHTS {
        match method {
            Method::Close | Method::Kill => {
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE
            }
            Method::Suspend => PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SUSPEND_RESUME,
            // Service/task methods never act on a process handle: query only.
            Method::Stop | Method::Disable => PROCESS_QUERY_LIMITED_INFORMATION,
        }
    }

    pub fn exe_path(&self) -> Option<String> {
        let mut buf = vec![0u16; 32768];
        let mut len = buf.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(self.handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
                .ok()?;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }

    pub fn creation_time(&self) -> Option<u64> {
        let (mut c, mut e, mut k, mut u) = Default::default();
        unsafe {
            GetProcessTimes(self.handle, &mut c, &mut e, &mut k, &mut u).ok()?;
        }
        Some(filetime_u64(c))
    }

    pub fn identity(&self) -> Option<Identity> {
        let exe = self.exe_path()?;
        let name = file_name(&exe).to_string();
        Some(Identity { name, exe, created: self.creation_time() })
    }

    /// Full command line via NtQueryInformationProcess(ProcessCommandLineInformation).
    /// `None` if unavailable (protected process, access denied, older OS).
    pub fn command_line(&self) -> Option<String> {
        command_line(self.handle)
    }

    /// True once the process has exited. Needs SYNCHRONIZE.
    pub fn has_exited(&self) -> bool {
        unsafe { WaitForSingleObject(self.handle, 0) == WAIT_OBJECT_0 }
    }

    /// Block until the process exits or `timeout` passes. Event-driven, no polling.
    pub fn wait_exit(&self, timeout: Duration) -> bool {
        let ms = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
        unsafe { WaitForSingleObject(self.handle, ms) == WAIT_OBJECT_0 }
    }

    pub fn terminate(&self) -> Result<(), String> {
        if self.pid == std::process::id() {
            return Err("refusing to terminate self".into());
        }
        unsafe {
            TerminateProcess(self.handle, FORCED_EXIT_CODE)
                .map_err(|e| format!("TerminateProcess failed: {}", e.message()))
        }
    }

    pub fn suspend(&self) -> Result<(), String> {
        if self.pid == std::process::id() {
            return Err("refusing to suspend self".into());
        }
        call_nt_process("NtSuspendProcess", self.handle)
    }

    pub fn resume(&self) -> Result<(), String> {
        call_nt_process("NtResumeProcess", self.handle)
    }
}

fn filetime_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// Final path component, accepting both separators.
pub fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// PID-reuse guard. A live process is the one we recorded only if name and exe path
/// match (case-insensitive) and, when both sides know it, the creation time matches.
pub fn identity_matches(recorded_name: &str, recorded_exe: &str, recorded_created: Option<u64>, live: &Identity) -> bool {
    if !recorded_name.eq_ignore_ascii_case(&live.name) {
        return false;
    }
    if !recorded_exe.eq_ignore_ascii_case(&live.exe) {
        return false;
    }
    match (recorded_created, live.created) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

/// Hard block on executables inside protected system directories.
pub fn is_protected_path(exe: &str) -> bool {
    let lower = exe.to_ascii_lowercase().replace('/', "\\");
    let sysroot = std::env::var("SystemRoot")
        .ok()
        .map(|s| format!("{}\\", s.trim_end_matches('\\')).to_ascii_lowercase());
    sysroot.iter().any(|s| lower.starts_with(s.as_str()))
        || PROTECTED_DIRS.iter().any(|d| lower.starts_with(&d.to_ascii_lowercase()))
}

// ---------------------------------------------------------------------------------------
// ntdll entry points
// ---------------------------------------------------------------------------------------

type NtProcessFn = unsafe extern "system" fn(*mut c_void) -> i32;
type NtQueryInformationProcessFn =
    unsafe extern "system" fn(*mut c_void, u32, *mut c_void, u32, *mut u32) -> i32;

fn ntdll_proc(name: windows::core::PCSTR) -> Option<unsafe extern "system" fn() -> isize> {
    unsafe {
        let module = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        GetProcAddress(module, name)
    }
}

fn call_nt_process(which: &str, handle: HANDLE) -> Result<(), String> {
    let proc = match which {
        "NtSuspendProcess" => ntdll_proc(s!("NtSuspendProcess")),
        "NtResumeProcess" => ntdll_proc(s!("NtResumeProcess")),
        _ => None,
    }
    .ok_or_else(|| format!("{which} not found in ntdll"))?;
    let status = unsafe {
        let f: NtProcessFn = std::mem::transmute(proc);
        f(handle.0)
    };
    if status >= 0 {
        Ok(())
    } else {
        Err(format!("{which} failed: NTSTATUS 0x{:08X}", status as u32))
    }
}

fn command_line(handle: HANDLE) -> Option<String> {
    const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;
    let proc = ntdll_proc(s!("NtQueryInformationProcess"))?;
    let query: NtQueryInformationProcessFn = unsafe { std::mem::transmute(proc) };
    let mut needed = 0u32;
    unsafe {
        query(handle.0, PROCESS_COMMAND_LINE_INFORMATION, std::ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 || needed > 1 << 20 {
        return None;
    }
    // u64 storage keeps the UNICODE_STRING header aligned.
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    let status = unsafe {
        query(
            handle.0,
            PROCESS_COMMAND_LINE_INFORMATION,
            buf.as_mut_ptr().cast(),
            (buf.len() * 8) as u32,
            &mut needed,
        )
    };
    if status < 0 {
        return None;
    }
    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *const u16,
    }
    unsafe {
        let us = &*(buf.as_ptr() as *const UnicodeString);
        if us.buffer.is_null() || us.length == 0 {
            return None;
        }
        let start = buf.as_ptr() as usize;
        let end = start + buf.len() * 8;
        let b = us.buffer as usize;
        let bytes = us.length as usize;
        // The string must lie inside the buffer we own.
        if b < start || b + bytes > end {
            return None;
        }
        let slice = std::slice::from_raw_parts(us.buffer, bytes / 2);
        Some(String::from_utf16_lossy(slice))
    }
}

// ---------------------------------------------------------------------------------------
// Windows: graceful close and the unsaved-data guard
// ---------------------------------------------------------------------------------------

struct WindowScan {
    pids: HashSet<u32>,
    found: Vec<(HWND, u32)>,
}

unsafe extern "system" fn collect_windows(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let scan = &mut *(lparam.0 as *mut WindowScan);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if scan.pids.contains(&pid) {
        scan.found.push((hwnd, pid));
    }
    BOOL(1)
}

/// Top-level windows owned by any of `pids`.
fn top_level_windows(pids: &HashSet<u32>) -> Vec<(HWND, u32)> {
    let mut scan = WindowScan { pids: pids.clone(), found: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(collect_windows), LPARAM(&mut scan as *mut WindowScan as isize));
    }
    scan.found
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Window classes the system adds to every GUI process; closing them means nothing.
fn is_system_helper_class(class: &str) -> bool {
    matches!(class, "IME" | "MSCTFIME UI" | "GDI+ Hook Window Class")
}

/// Post WM_CLOSE to each unowned top-level window of `pids`, like `taskkill` without /F.
/// Returns how many windows were asked to close.
pub fn post_close(pids: &HashSet<u32>) -> usize {
    let mut posted = 0;
    for (hwnd, _) in top_level_windows(pids) {
        let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.map(|o| !o.0.is_null()).unwrap_or(false);
        if owned || is_system_helper_class(&class_name(hwnd)) {
            continue;
        }
        if unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }.is_ok() {
            posted += 1;
        }
    }
    posted
}

/// Unsaved-data guard: after WM_CLOSE, a visible dialog (`#32770`) or a visible owned
/// popup usually means "Save changes?". Forcing then would lose the user's work.
pub fn shows_dialog(pids: &HashSet<u32>) -> bool {
    top_level_windows(pids).into_iter().any(|(hwnd, _)| unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        let owned = GetWindow(hwnd, GW_OWNER).map(|o| !o.0.is_null()).unwrap_or(false);
        owned || class_name(hwnd) == "#32770"
    })
}

/// Wait for every handle to exit, sharing one deadline. Returns PIDs still alive.
pub fn wait_all_exit(handles: &[&ProcHandle], deadline: Instant) -> Vec<u32> {
    let mut alive = Vec::new();
    for h in handles {
        let left = deadline.saturating_duration_since(Instant::now());
        if !h.wait_exit(left) {
            alive.push(h.pid);
        }
    }
    alive
}

// ---------------------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RestoreStatus {
    Restored,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RestoreResult {
    pub status: RestoreStatus,
    pub detail: String,
    /// PID of a relaunched process.
    pub new_pid: Option<u32>,
}

impl RestoreResult {
    fn new(status: RestoreStatus, detail: impl Into<String>) -> Self {
        Self { status, detail: detail.into(), new_pid: None }
    }
}

/// The live process at `pid`, if it is still the one the entry recorded.
fn same_process(entry: &Entry, rights: PROCESS_ACCESS_RIGHTS) -> Option<ProcHandle> {
    let h = ProcHandle::open(entry.pid, rights).ok()?;
    let id = h.identity()?;
    identity_matches(&entry.name, &entry.exe, entry.created, &id).then_some(h)
}

/// Undo one journal entry. Never panics; every outcome is described.
pub fn restore(entry: &Entry, self_pid: u32) -> RestoreResult {
    use RestoreStatus::*;
    if entry.state == EntryState::Failed {
        return RestoreResult::new(Skipped, "action never happened");
    }
    match entry.method {
        Method::Suspend => {
            let Some(h) = same_process(entry, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SUSPEND_RESUME) else {
                return RestoreResult::new(Skipped, "process is gone or its PID was reused; nothing to resume");
            };
            // Resuming a process that was never suspended (crash before the action) is a
            // no-op, so Planned entries are safe to resume too.
            match h.resume() {
                Ok(()) => RestoreResult::new(Restored, "resumed"),
                Err(e) => RestoreResult::new(Failed, e),
            }
        }
        Method::Close | Method::Kill => {
            if same_process(entry, PROCESS_QUERY_LIMITED_INFORMATION).is_some() {
                return RestoreResult::new(Skipped, "original process is still running");
            }
            if !entry.relaunch {
                return RestoreResult::new(Skipped, "child process; its parent starts it again");
            }
            if !Path::new(&entry.exe).is_file() {
                return RestoreResult::new(Skipped, format!("executable no longer exists: {}", entry.exe));
            }
            if is_protected_path(&entry.exe) {
                return RestoreResult::new(Skipped, "executable is in a protected directory");
            }
            if let Some(pid) = running_with_exe(&entry.exe, self_pid) {
                return RestoreResult::new(Skipped, format!("already running again (pid {pid})"));
            }
            match relaunch(&entry.exe, entry.command_line.as_deref()) {
                Ok(pid) => RestoreResult { status: Restored, detail: "relaunched".into(), new_pid: Some(pid) },
                Err(e) => RestoreResult::new(Failed, e),
            }
        }
        // Process entries never carry these (toggle journals services and tasks separately).
        Method::Stop | Method::Disable => RestoreResult::new(Skipped, "not a process action"),
    }
}

/// Any process other than `exclude` running from exactly `exe`.
pub fn running_with_exe(exe: &str, exclude: u32) -> Option<u32> {
    let base = file_name(exe);
    crate::process::list_processes()
        .into_iter()
        .filter(|p| p.pid != exclude && p.name.eq_ignore_ascii_case(base))
        .find(|p| {
            ProcHandle::open(p.pid, PROCESS_QUERY_LIMITED_INFORMATION)
                .ok()
                .and_then(|h| h.exe_path())
                .is_some_and(|path| path.eq_ignore_ascii_case(exe))
        })
        .map(|p| p.pid)
}

/// Split a Windows command line into argv[0] and the untouched remainder.
/// argv[0] follows the CreateProcess rule: quoted up to the next quote, else up to the
/// first space or tab. No escape processing applies to argv[0].
pub fn split_command_line(cmd: &str) -> (&str, &str) {
    let cmd = cmd.trim_start();
    let (first, rest) = if let Some(stripped) = cmd.strip_prefix('"') {
        match stripped.find('"') {
            Some(i) => (&stripped[..i], &stripped[i + 1..]),
            None => (stripped, ""),
        }
    } else {
        match cmd.find([' ', '\t']) {
            Some(i) => (&cmd[..i], &cmd[i..]),
            None => (cmd, ""),
        }
    };
    (first, rest.trim_start())
}

/// Start `exe` again with the recorded arguments (exe alone if none were recorded).
/// Arguments are passed through verbatim so the original quoting survives.
pub fn relaunch(exe: &str, command_line: Option<&str>) -> Result<u32, String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let mut cmd = Command::new(exe);
    if let Some(line) = command_line {
        let (_, args) = split_command_line(line);
        if !args.is_empty() {
            cmd.raw_arg(args);
        }
    }
    if let Some(dir) = Path::new(exe).parent() {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NEW_PROCESS_GROUP);
    // Dropping the Child does not kill it; the app keeps running on its own.
    cmd.spawn().map(|c| c.id()).map_err(|e| format!("relaunch failed: {e}"))
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

/// A harmless child process for tests: a copy of this test binary under a unique name,
/// started so it runs only `sleeper_child_entry`. It never touches anything else and
/// exits by itself after 60 s.
#[cfg(test)]
pub(crate) mod testutil {
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Integration tests that spawn and act on sleepers run one at a time.
    pub static SERIAL: Mutex<()> = Mutex::new(());

    pub struct Sleeper {
        pub child: Child,
        pub exe: PathBuf,
        pub name: String,
        dir: PathBuf,
    }

    impl Sleeper {
        pub fn pid(&self) -> u32 {
            self.child.id()
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            // Restore may have relaunched copies of this exe; stop them too.
            for p in crate::process::list_processes() {
                if p.name.eq_ignore_ascii_case(&self.name) {
                    if let Ok(h) = super::ProcHandle::open(p.pid, super::ProcHandle::rights_for(crate::Method::Kill)) {
                        if h.exe_path().is_some_and(|e| e.eq_ignore_ascii_case(&self.exe.to_string_lossy())) {
                            let _ = h.terminate();
                            h.wait_exit(Duration::from_secs(5));
                        }
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// `mode`: "sleep" (no windows) or "window" (one hidden window that closes on WM_CLOSE).
    pub fn spawn(mode: &str) -> Sleeper {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        // The mode is encoded in the file name so a relaunched copy (which gets no
        // test-only environment) behaves the same way.
        let name = format!("hmr-sleeper-{mode}-{}-{}.exe", std::process::id(), n);
        let dir = crate::journal::tests::temp_dir("sleeper");
        let exe = dir.join(&name);
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        let ready = dir.join("ready");
        let child = Command::new(&exe)
            .args(["--exact", "actions::testutil::sleeper_child_entry", "--test-threads=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Test-only wait for the child to signal readiness.
        let start = Instant::now();
        while !ready.exists() && start.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(ready.exists(), "sleeper did not start");
        Sleeper { child, exe, name, dir }
    }

    /// Entry point executed inside the sleeper copy. A no-op in the normal test run.
    #[test]
    fn sleeper_child_entry() {
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().to_string();
        if !name.starts_with("hmr-sleeper-") {
            return;
        }
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(60));
            std::process::exit(0);
        });
        let ready = exe.parent().unwrap().join("ready");
        if name.starts_with("hmr-sleeper-window-") {
            use windows::core::w;
            use windows::Win32::UI::WindowsAndMessaging::*;
            unsafe {
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE(0), w!("STATIC"), w!("hmr-sleeper"), WINDOW_STYLE(0),
                    0, 0, 10, 10, None, None, None, None,
                )
                .unwrap();
                std::fs::write(&ready, b"1").unwrap();
                let mut msg = MSG::default();
                // DefWindowProc turns WM_CLOSE into DestroyWindow; exit once that happens.
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                    if !IsWindow(Some(hwnd)).as_bool() {
                        break;
                    }
                }
            }
            std::process::exit(0);
        }
        std::fs::write(&ready, b"1").unwrap();
        std::thread::sleep(Duration::from_secs(60));
        std::process::exit(0);
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    fn id(name: &str, exe: &str, created: Option<u64>) -> Identity {
        Identity { name: name.into(), exe: exe.into(), created }
    }

    #[test]
    fn pid_reuse_guard() {
        let live = id("App.exe", r"C:\Apps\App.exe", Some(100));
        assert!(identity_matches("app.exe", r"c:\apps\app.exe", Some(100), &live));
        assert!(identity_matches("app.exe", r"C:\Apps\App.exe", None, &live), "older journal without time");
        assert!(!identity_matches("app.exe", r"C:\Apps\App.exe", Some(101), &live), "PID reused by a newer instance");
        assert!(!identity_matches("other.exe", r"C:\Apps\App.exe", Some(100), &live));
        assert!(!identity_matches("app.exe", r"D:\Elsewhere\App.exe", Some(100), &live));
    }

    #[test]
    fn protected_directories() {
        assert!(is_protected_path(r"C:\Windows\System32\svchost.exe"));
        assert!(is_protected_path(r"c:\windows\explorer.exe"));
        assert!(is_protected_path("C:/Windows/System32/ping.exe"));
        assert!(is_protected_path(r"C:\Program Files\Windows Defender\MsMpEng.exe"));
        assert!(!is_protected_path(r"C:\Users\me\AppData\Local\Discord\app-1.0\Discord.exe"));
        assert!(!is_protected_path(r"C:\WindowsApps-not-really\x.exe"));
    }

    #[test]
    fn command_line_split() {
        assert_eq!(split_command_line(r#""C:\Program Files\A\a.exe" --x "y z""#), (r"C:\Program Files\A\a.exe", r#"--x "y z""#));
        assert_eq!(split_command_line(r"C:\A\a.exe   -n 5"), (r"C:\A\a.exe", "-n 5"));
        assert_eq!(split_command_line(r"a.exe"), ("a.exe", ""));
        assert_eq!(split_command_line(r#""C:\A\a.exe""#), (r"C:\A\a.exe", ""));
    }

    #[test]
    fn file_name_handles_both_separators() {
        assert_eq!(file_name(r"C:\A\b.exe"), "b.exe");
        assert_eq!(file_name("C:/A/b.exe"), "b.exe");
        assert_eq!(file_name("b.exe"), "b.exe");
    }

    #[test]
    fn refuses_system_pids() {
        assert!(ProcHandle::open(0, PROCESS_QUERY_LIMITED_INFORMATION).is_err());
        assert!(ProcHandle::open(4, PROCESS_QUERY_LIMITED_INFORMATION).is_err());
    }

    #[test]
    fn reads_identity_and_command_line_of_child() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("sleep");
        let h = ProcHandle::open(s.pid(), PROCESS_QUERY_LIMITED_INFORMATION).unwrap();
        let ident = h.identity().unwrap();
        assert!(ident.exe.eq_ignore_ascii_case(&s.exe.to_string_lossy()));
        assert!(ident.name.eq_ignore_ascii_case(&s.name));
        assert!(ident.created.is_some());
        let cmd = h.command_line().expect("command line readable for own child");
        assert!(cmd.contains("sleeper_child_entry"), "{cmd}");
    }

    #[test]
    fn kill_terminates_child() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("sleep");
        let h = ProcHandle::open(s.pid(), ProcHandle::rights_for(Method::Kill)).unwrap();
        assert!(!h.has_exited());
        h.terminate().unwrap();
        assert!(h.wait_exit(Duration::from_secs(5)));
    }

    #[test]
    fn close_is_graceful_for_a_window_that_accepts_wm_close() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("window");
        let h = ProcHandle::open(s.pid(), ProcHandle::rights_for(Method::Close)).unwrap();
        let pids = HashSet::from([s.pid()]);
        assert!(post_close(&pids) >= 1);
        assert!(h.wait_exit(Duration::from_secs(10)), "window app should exit on WM_CLOSE");
        assert!(!shows_dialog(&pids));
    }

    #[test]
    fn close_without_windows_times_out_then_force_works() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("sleep");
        let h = ProcHandle::open(s.pid(), ProcHandle::rights_for(Method::Close)).unwrap();
        let pids = HashSet::from([s.pid()]);
        assert_eq!(post_close(&pids), 0);
        let alive = wait_all_exit(&[&h], Instant::now() + Duration::from_millis(300));
        assert_eq!(alive, vec![s.pid()]);
        h.terminate().unwrap();
        assert!(h.wait_exit(Duration::from_secs(5)));
    }

    #[test]
    fn suspend_and_resume_child() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("sleep");
        let h = ProcHandle::open(s.pid(), ProcHandle::rights_for(Method::Suspend)).unwrap();
        h.suspend().unwrap();
        h.resume().unwrap();
        // Still the same process and still alive.
        let k = ProcHandle::open(s.pid(), ProcHandle::rights_for(Method::Kill)).unwrap();
        assert!(!k.has_exited());
    }

    #[test]
    fn restore_relaunches_killed_child_and_skips_when_running() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("sleep");
        let h = ProcHandle::open(s.pid(), ProcHandle::rights_for(Method::Kill)).unwrap();
        let ident = h.identity().unwrap();
        let entry = Entry {
            seq: 0,
            pid: s.pid(),
            name: ident.name.clone(),
            exe: ident.exe.clone(),
            command_line: h.command_line(),
            created: ident.created,
            method: Method::Kill,
            state: EntryState::Done,
            relaunch: true,
            note: None,
        };
        // Still alive: restore must not start a second copy.
        let r = restore(&entry, std::process::id());
        assert_eq!(r.status, RestoreStatus::Skipped, "{r:?}");

        h.terminate().unwrap();
        assert!(h.wait_exit(Duration::from_secs(5)));
        let r = restore(&entry, std::process::id());
        assert_eq!(r.status, RestoreStatus::Restored, "{r:?}");
        let new_pid = r.new_pid.unwrap();
        assert_ne!(new_pid, s.pid());
        assert_eq!(running_with_exe(&entry.exe, std::process::id()), Some(new_pid));

        // A second restore sees the relaunched copy and does nothing.
        let r2 = restore(&entry, std::process::id());
        assert_eq!(r2.status, RestoreStatus::Skipped, "{r2:?}");
        // Sleeper's Drop stops the relaunched copy.
    }

    #[test]
    fn restore_resume_refuses_a_different_process() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let s = spawn("sleep");
        let h = ProcHandle::open(s.pid(), ProcHandle::rights_for(Method::Suspend)).unwrap();
        let ident = h.identity().unwrap();
        let mut entry = Entry {
            seq: 0,
            pid: s.pid(),
            name: ident.name.clone(),
            exe: ident.exe.clone(),
            command_line: None,
            created: ident.created.map(|c| c + 1), // pretend PID was reused
            method: Method::Suspend,
            state: EntryState::Done,
            relaunch: false,
            note: None,
        };
        h.suspend().unwrap();
        let r = restore(&entry, std::process::id());
        assert_eq!(r.status, RestoreStatus::Skipped, "{r:?}");
        entry.created = ident.created;
        let r = restore(&entry, std::process::id());
        assert_eq!(r.status, RestoreStatus::Restored, "{r:?}");
    }

    #[test]
    fn restore_skips_missing_exe() {
        let entry = Entry {
            seq: 0,
            pid: 0,
            name: "gone.exe".into(),
            exe: r"C:\definitely\not\here\gone.exe".into(),
            command_line: None,
            created: None,
            method: Method::Close,
            state: EntryState::Done,
            relaunch: true,
            note: None,
        };
        let r = restore(&entry, std::process::id());
        assert_eq!(r.status, RestoreStatus::Skipped);
        assert!(r.detail.contains("no longer exists"));
    }
}
