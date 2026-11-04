//! The real [`AdminBackend`]: the Service Control Manager and the Task Scheduler 2.0 COM
//! API. Used only inside the elevated helper. It does exactly what the handler asks and
//! never deletes or edits a service or task definition: the only writes are
//! ControlService(STOP), StartService, ChangeServiceConfig(start type only),
//! ChangeServiceConfig2(delayed-auto flag only) and IRegisteredTask::put_Enabled.
//!
//! Each handle is opened with only the rights that one call needs. Access denied while
//! elevated is reported as `locked` (some services deny even administrators).
//!
//! Waiting for a stop or start is a bounded status loop (QueryServiceStatusEx every
//! 250 ms, at most the request's timeout) inside a request the user made, in a process
//! that exits afterwards; nothing polls in the background.

use super::handler::AdminBackend;
use super::proto::*;
use std::time::{Duration, Instant};

use windows::core::{BSTR, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::Services::*;
use windows::Win32::System::TaskScheduler::{IRegisteredTask, ITaskService, TaskScheduler};
use windows::Win32::System::Variant::VARIANT;

const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_INVALID_NAME: u32 = 123;
const ERROR_DEPENDENT_SERVICES_RUNNING: u32 = 1051;
const ERROR_INVALID_SERVICE_CONTROL: u32 = 1052;
const ERROR_SERVICE_ALREADY_RUNNING: u32 = 1056;
const ERROR_SERVICE_DISABLED: u32 = 1058;
const ERROR_SERVICE_DOES_NOT_EXIST: u32 = 1060;
const ERROR_SERVICE_CANNOT_ACCEPT_CTRL: u32 = 1061;
const ERROR_SERVICE_NOT_ACTIVE: u32 = 1062;
const ERROR_SERVICE_MARKED_FOR_DELETE: u32 = 1072;

/// Win32 error code inside an HRESULT (0 if it is not a Win32 HRESULT).
fn win32(e: &windows::core::Error) -> u32 {
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        hr & 0xFFFF
    } else {
        0
    }
}

fn map_err(what: &str, target: &str, e: windows::core::Error) -> AdminError {
    let code = match win32(&e) {
        ERROR_ACCESS_DENIED => ErrorCode::Locked,
        ERROR_SERVICE_DOES_NOT_EXIST | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_SERVICE_MARKED_FOR_DELETE => ErrorCode::NotFound,
        ERROR_INVALID_NAME => ErrorCode::BadRequest,
        ERROR_INVALID_SERVICE_CONTROL => ErrorCode::NotStoppable,
        _ => ErrorCode::Failed,
    };
    let msg = if code == ErrorCode::Locked {
        format!("Windows denied {what} \"{target}\" even to an administrator")
    } else {
        format!("{what} \"{target}\" failed: {} ({:#010x})", e.message(), e.code().0 as u32)
    };
    AdminError::new(code, msg)
}

struct Sc(SC_HANDLE);

impl Drop for Sc {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

pub struct WinBackend {
    scm: Sc,
    tasks: Option<ITaskService>,
    com: bool,
}

impl WinBackend {
    /// Connect to the local SCM (connect + enumerate rights only; each service is opened
    /// with the minimum rights per call) and initialise COM (MTA) on this thread for the
    /// Task Scheduler. Call from the thread that will handle requests.
    pub fn new() -> Result<WinBackend, AdminError> {
        let scm = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
            .map_err(|e| map_err("opening", "the service manager", e))?;
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        Ok(WinBackend { scm: Sc(scm), tasks: None, com })
    }

    fn open(&self, name: &str, access: u32, what: &str) -> Result<Sc, AdminError> {
        let n = HSTRING::from(name);
        unsafe { OpenServiceW(self.scm.0, &n, access) }.map(Sc).map_err(|e| map_err(what, name, e))
    }

    fn status(svc: &Sc, name: &str) -> Result<SERVICE_STATUS_PROCESS, AdminError> {
        let mut st = SERVICE_STATUS_PROCESS::default();
        let mut needed = 0u32;
        let buf = unsafe {
            std::slice::from_raw_parts_mut(&mut st as *mut _ as *mut u8, std::mem::size_of::<SERVICE_STATUS_PROCESS>())
        };
        unsafe { QueryServiceStatusEx(svc.0, SC_STATUS_PROCESS_INFO, Some(buf), &mut needed) }
            .map_err(|e| map_err("reading the status of", name, e))?;
        Ok(st)
    }

    /// (start type, display name) or None if the config is unreadable.
    fn config(svc: &Sc) -> Option<(u32, String)> {
        let mut needed = 0u32;
        let _ = unsafe { QueryServiceConfigW(svc.0, None, 0, &mut needed) };
        if needed == 0 {
            return None;
        }
        let mut buf: Vec<u64> = vec![0; (needed as usize).div_ceil(8)];
        let cfg = buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW;
        unsafe { QueryServiceConfigW(svc.0, Some(cfg), (buf.len() * 8) as u32, &mut needed) }.ok()?;
        let cfg = unsafe { &*cfg };
        let display = if cfg.lpDisplayName.is_null() { String::new() } else { unsafe { cfg.lpDisplayName.to_string() }.unwrap_or_default() };
        Some((cfg.dwStartType.0, display))
    }

    fn delayed(svc: &Sc) -> Option<bool> {
        let mut info = SERVICE_DELAYED_AUTO_START_INFO::default();
        let mut needed = 0u32;
        let buf = unsafe {
            std::slice::from_raw_parts_mut(&mut info as *mut _ as *mut u8, std::mem::size_of::<SERVICE_DELAYED_AUTO_START_INFO>())
        };
        unsafe { QueryServiceConfig2W(svc.0, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, Some(buf), &mut needed) }.ok()?;
        Some(info.fDelayedAutostart.as_bool())
    }

    /// Canonical key name for a display name (the SCM matches names case-insensitively;
    /// this gives the real spelling).
    fn key_name(&self, display: &str) -> Option<String> {
        if display.is_empty() {
            return None;
        }
        let d = HSTRING::from(display);
        let mut buf = vec![0u16; 512];
        let mut len = buf.len() as u32;
        unsafe { GetServiceKeyNameW(self.scm.0, &d, Some(PWSTR(buf.as_mut_ptr())), &mut len) }.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }

    fn wait_for(&self, svc: &Sc, name: &str, want: SERVICE_STATUS_CURRENT_STATE, timeout: Duration) -> Result<(), AdminError> {
        let deadline = Instant::now() + timeout;
        loop {
            let st = Self::status(svc, name)?;
            if st.dwCurrentState == want {
                return Ok(());
            }
            if want == SERVICE_RUNNING && st.dwCurrentState == SERVICE_STOPPED {
                return Err(AdminError::new(
                    ErrorCode::Failed,
                    format!("\"{name}\" stopped again right after starting (exit code {})", st.dwWin32ExitCode),
                ));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(AdminError::new(ErrorCode::Timeout, format!("\"{name}\" did not reach the wanted state in {timeout:?}")));
            }
            std::thread::sleep(left.min(Duration::from_millis(250)));
        }
    }

    fn task_service(&mut self) -> Result<ITaskService, AdminError> {
        if let Some(s) = &self.tasks {
            return Ok(s.clone());
        }
        let svc: ITaskService = unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| map_err("opening", "the Task Scheduler", e))?;
        let empty = VARIANT::default();
        unsafe { svc.Connect(&empty, &empty, &empty, &empty) }.map_err(|e| map_err("connecting to", "the Task Scheduler", e))?;
        self.tasks = Some(svc.clone());
        Ok(svc)
    }

    fn task(&mut self, path: &str) -> Result<IRegisteredTask, AdminError> {
        let svc = self.task_service()?;
        let (folder, name) = match path.rsplit_once('\\') {
            Some(("", n)) => ("\\", n),
            Some((f, n)) => (f, n),
            None => return Err(AdminError::new(ErrorCode::BadRequest, "task path must start with \\")),
        };
        let f = unsafe { svc.GetFolder(&BSTR::from(folder)) }.map_err(|e| map_err("opening the task folder of", path, e))?;
        unsafe { f.GetTask(&BSTR::from(name)) }.map_err(|e| map_err("opening the task", path, e))
    }
}

impl Drop for WinBackend {
    fn drop(&mut self) {
        self.tasks = None;
        if self.com {
            unsafe { CoUninitialize() };
        }
    }
}

impl AdminBackend for WinBackend {
    fn service_query(&mut self, name: &str) -> Result<ServiceState, AdminError> {
        let (svc, has_config) = match self.open(name, SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG, "reading") {
            Ok(s) => (s, true),
            Err(e) if e.code == ErrorCode::Locked => (self.open(name, SERVICE_QUERY_STATUS, "reading")?, false),
            Err(e) => return Err(e),
        };
        let st = Self::status(&svc, name)?;
        let cfg = if has_config { Self::config(&svc) } else { None };
        let delayed = if has_config { Self::delayed(&svc) } else { None };
        let display = cfg.as_ref().map(|c| c.1.clone()).unwrap_or_default();
        // Used for the never-touch re-check only; every action uses the requested name.
        let canonical = self.key_name(&display).unwrap_or_else(|| name.to_string());
        let state = RunState::from_raw(st.dwCurrentState.0);
        Ok(ServiceState {
            name: canonical,
            display,
            state,
            running: state != RunState::Stopped,
            start: cfg.map(|c| StartType::from_raw(c.0)).unwrap_or(StartType::Unknown),
            delayed_auto: delayed,
            service_type: st.dwServiceType.0,
            pid: st.dwProcessId,
            protected: false,
        })
    }

    fn service_dependents(&mut self, name: &str) -> Result<Vec<String>, AdminError> {
        let svc = self.open(name, SERVICE_ENUMERATE_DEPENDENTS, "listing the dependents of")?;
        let mut needed = 0u32;
        let mut count = 0u32;
        if unsafe { EnumDependentServicesW(svc.0, SERVICE_STATE_ALL, None, 0, &mut needed, &mut count) }.is_ok() {
            return Ok(Vec::new()); // no dependents
        }
        if needed == 0 {
            return Err(AdminError::new(ErrorCode::Failed, format!("listing the dependents of \"{name}\" failed")));
        }
        let mut buf: Vec<u64> = vec![0; (needed as usize).div_ceil(8)];
        let p = buf.as_mut_ptr() as *mut ENUM_SERVICE_STATUSW;
        unsafe { EnumDependentServicesW(svc.0, SERVICE_STATE_ALL, Some(p), (buf.len() * 8) as u32, &mut needed, &mut count) }
            .map_err(|e| map_err("listing the dependents of", name, e))?;
        let entries = unsafe { std::slice::from_raw_parts(p, count as usize) };
        // Documented order: reverse of start order, which is a safe stop order.
        Ok(entries
            .iter()
            .filter(|e| !e.lpServiceName.is_null())
            .filter_map(|e| unsafe { e.lpServiceName.to_string() }.ok())
            .collect())
    }

    fn service_stop(&mut self, name: &str, timeout: Duration) -> Result<(), AdminError> {
        let svc = self.open(name, SERVICE_STOP | SERVICE_QUERY_STATUS, "stopping")?;
        let mut st = SERVICE_STATUS::default();
        match unsafe { ControlService(svc.0, SERVICE_CONTROL_STOP, &mut st) } {
            Ok(()) => {}
            Err(e) => match win32(&e) {
                ERROR_SERVICE_NOT_ACTIVE => return Ok(()),
                ERROR_SERVICE_CANNOT_ACCEPT_CTRL if Self::status(&svc, name)?.dwCurrentState == SERVICE_STOP_PENDING => {}
                ERROR_DEPENDENT_SERVICES_RUNNING => {
                    return Err(AdminError::new(ErrorCode::Failed, format!("\"{name}\" still has running dependents")))
                }
                _ => return Err(map_err("stopping", name, e)),
            },
        }
        self.wait_for(&svc, name, SERVICE_STOPPED, timeout)
    }

    fn service_start(&mut self, name: &str, timeout: Duration) -> Result<(), AdminError> {
        let svc = self.open(name, SERVICE_START | SERVICE_QUERY_STATUS, "starting")?;
        match unsafe { StartServiceW(svc.0, None) } {
            Ok(()) => {}
            Err(e) => match win32(&e) {
                ERROR_SERVICE_ALREADY_RUNNING => {}
                ERROR_SERVICE_DISABLED => {
                    return Err(AdminError::new(ErrorCode::Failed, format!("\"{name}\" is disabled and cannot be started")))
                }
                _ => return Err(map_err("starting", name, e)),
            },
        }
        self.wait_for(&svc, name, SERVICE_RUNNING, timeout)
    }

    fn service_set_start(&mut self, name: &str, start: StartChoice) -> Result<(), AdminError> {
        let svc = self.open(name, SERVICE_CHANGE_CONFIG | SERVICE_QUERY_CONFIG, "changing the start type of")?;
        let raw = match start {
            StartChoice::Auto | StartChoice::AutoDelayed => SERVICE_AUTO_START,
            StartChoice::Manual => SERVICE_DEMAND_START,
            StartChoice::Disabled => SERVICE_DISABLED,
        };
        unsafe {
            ChangeServiceConfigW(
                svc.0,
                ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
                raw,
                SERVICE_ERROR(SERVICE_NO_CHANGE),
                PCWSTR::null(),
                PCWSTR::null(),
                None,
                PCWSTR::null(),
                PCWSTR::null(),
                PCWSTR::null(),
                PCWSTR::null(),
            )
        }
        .map_err(|e| map_err("changing the start type of", name, e))?;
        let delayed = match start {
            StartChoice::Auto => Some(false),
            StartChoice::AutoDelayed => Some(true),
            _ => None,
        };
        if let Some(d) = delayed {
            let info = SERVICE_DELAYED_AUTO_START_INFO { fDelayedAutostart: d.into() };
            unsafe { ChangeServiceConfig2W(svc.0, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, Some(&info as *const _ as *const _)) }
                .map_err(|e| map_err("changing the delayed-start flag of", name, e))?;
        }
        Ok(())
    }

    fn task_query(&mut self, path: &str) -> Result<TaskState, AdminError> {
        let t = self.task(path)?;
        let canonical = unsafe { t.Path() }.map(|b| b.to_string()).map_err(|e| map_err("reading the path of", path, e))?;
        let enabled = unsafe { t.Enabled() }.map_err(|e| map_err("reading", path, e))?;
        Ok(TaskState { path: canonical, enabled: enabled.as_bool(), protected: false })
    }

    fn task_set_enabled(&mut self, path: &str, enabled: bool) -> Result<(), AdminError> {
        let t = self.task(path)?;
        unsafe { t.SetEnabled(VARIANT_BOOL::from(enabled)) }.map_err(|e| map_err(if enabled { "enabling" } else { "disabling" }, path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read-only: queries need no admin for most services and change nothing. Runs on
    /// the real SCM and Task Scheduler.
    #[test]
    fn read_only_queries_work_unelevated() {
        let mut b = WinBackend::new().unwrap();
        let s = b.service_query("rpcss").unwrap();
        assert_eq!(s.name, "RpcSs", "canonical spelling");
        assert!(s.running);
        assert_eq!(s.start, StartType::Auto);
        let e = b.service_query("HoldMyRAM-no-such-service").unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        // RpcSs has many dependents; the list must be non-empty and well-formed. Listing
        // dependents needs admin, so unelevated it must come back as a clean refusal.
        match b.service_dependents("RpcSs") {
            Ok(deps) => {
                assert!(!deps.is_empty());
                assert!(deps.iter().all(|d| !d.is_empty()));
            }
            Err(e) => {
                assert!(!crate::admin::sys::is_elevated(), "{e}");
                assert_eq!(e.code, ErrorCode::Locked);
            }
        }
        let t = b.task_query(r"\Microsoft\Windows\Defrag\ScheduledDefrag");
        if let Ok(t) = t {
            assert_eq!(t.path, r"\Microsoft\Windows\Defrag\ScheduledDefrag");
        }
        assert_eq!(b.task_query(r"\HoldMyRAM no such task").unwrap_err().code, ErrorCode::NotFound);
    }
}
