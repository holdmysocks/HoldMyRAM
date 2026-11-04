//! App side: launch the elevated helper (one UAC prompt), talk to it over the pipe, and
//! expose typed calls. Nothing elevated runs unless [`Helper::arm`] is called, and the
//! helper exits when this handle is disarmed or dropped, or when the app exits.

use super::handler::{AdminBackend, Handler};
use super::pipe::{self, HelperArgs, Nonce, PipeServer};
use super::proto::*;
use super::sys;
use std::os::windows::io::OwnedHandle;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows::Win32::System::Threading::GetProcessId;
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

pub const HELPER_EXE: &str = "holdmyram-helper.exe";

/// How long arm() waits for the helper to connect after the UAC prompt was accepted.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

fn reply_timeout(op: &Op) -> Duration {
    match op {
        // The helper's own stop budget is 60 s plus a possible rollback.
        Op::ServiceStop { .. } => Duration::from_secs(150),
        Op::ServiceStart { .. } => Duration::from_secs(45),
        _ => Duration::from_secs(20),
    }
}

fn err(code: ErrorCode, m: impl Into<String>) -> AdminError {
    AdminError::new(code, m)
}

fn unexpected(r: Reply) -> AdminError {
    err(ErrorCode::Transport, format!("unexpected reply {r:?}"))
}

/// Typed operations, implemented by the real [`Helper`] and by [`LocalApi`] (tests).
pub trait AdminApi {
    /// Send one op, get its reply.
    fn call(&mut self, op: Op) -> Result<Reply, AdminError>;

    fn ping(&mut self) -> Result<u32, AdminError> {
        match self.call(Op::Ping)? {
            Reply::Pong { version, .. } => Ok(version),
            r => Err(unexpected(r)),
        }
    }
    fn service_query(&mut self, name: &str) -> Result<ServiceState, AdminError> {
        match self.call(Op::ServiceQuery { name: name.into() })? {
            Reply::Service(s) => Ok(s),
            r => Err(unexpected(r)),
        }
    }
    fn service_stop(&mut self, name: &str) -> Result<StopReport, AdminError> {
        match self.call(Op::ServiceStop { name: name.into() })? {
            Reply::Stopped(s) => Ok(s),
            r => Err(unexpected(r)),
        }
    }
    fn service_start(&mut self, name: &str) -> Result<StartReport, AdminError> {
        match self.call(Op::ServiceStart { name: name.into() })? {
            Reply::Started(s) => Ok(s),
            r => Err(unexpected(r)),
        }
    }
    fn service_set_start_type(&mut self, name: &str, start: StartChoice) -> Result<StartTypeReport, AdminError> {
        match self.call(Op::ServiceSetStartType { name: name.into(), start })? {
            Reply::StartTypeSet(s) => Ok(s),
            r => Err(unexpected(r)),
        }
    }
    fn task_query(&mut self, path: &str) -> Result<TaskState, AdminError> {
        match self.call(Op::TaskQuery { path: path.into() })? {
            Reply::Task(t) => Ok(t),
            r => Err(unexpected(r)),
        }
    }
    fn task_set_enabled(&mut self, path: &str, enabled: bool) -> Result<TaskReport, AdminError> {
        match self.call(Op::TaskSetEnabled { path: path.into(), enabled })? {
            Reply::TaskSet(t) => Ok(t),
            r => Err(unexpected(r)),
        }
    }
}

/// In-process stand-in for the helper: requests go through the same JSON encoding and
/// the same handler, without a pipe or elevation. For tests and dry runs.
pub struct LocalApi<B: AdminBackend> {
    pub handler: Handler<B>,
    next_id: u64,
}

impl<B: AdminBackend> LocalApi<B> {
    pub fn new(handler: Handler<B>) -> Self {
        LocalApi { handler, next_id: 1 }
    }
}

impl<B: AdminBackend> AdminApi for LocalApi<B> {
    fn call(&mut self, op: Op) -> Result<Reply, AdminError> {
        let id = self.next_id;
        self.next_id += 1;
        let line = request_line(&Request { id, op });
        let (resp, _) = self.handler.handle_line(line.trim_end().as_bytes());
        let resp = parse_response(resp.to_line().trim_end().as_bytes())?;
        if resp.id != id {
            return Err(err(ErrorCode::Transport, "response id mismatch"));
        }
        resp.into_result()
    }
}

struct Conn {
    server: PipeServer,
    child: OwnedHandle,
}

/// Handle to the elevated helper. Not armed until [`Helper::arm`] succeeds.
pub struct Helper {
    exe: PathBuf,
    conn: Option<Conn>,
    next_id: u64,
}

impl Default for Helper {
    fn default() -> Self {
        Self::new()
    }
}

impl Helper {
    /// The helper next to the running exe.
    pub fn new() -> Helper {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join(HELPER_EXE)))
            .unwrap_or_else(|| PathBuf::from(HELPER_EXE));
        Helper::with_exe(exe)
    }

    pub fn with_exe(exe: impl Into<PathBuf>) -> Helper {
        Helper { exe: exe.into(), conn: None, next_id: 1 }
    }

    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// Launch the helper elevated (UAC prompt; blocks until the user answers) and
    /// complete the handshake. No-op if already armed. `UacCancelled` if the user
    /// declined; `Handshake` if the process that connected is not the one launched or
    /// its proof is wrong.
    pub fn arm(&mut self) -> Result<(), AdminError> {
        if self.is_armed() {
            return Ok(());
        }
        if !self.exe.is_file() {
            return Err(err(ErrorCode::Failed, format!("helper not found at {}", self.exe.display())));
        }
        let server = PipeServer::create().map_err(|e| err(ErrorCode::Failed, format!("could not create the admin pipe: {e}")))?;
        let nonce = pipe::new_nonce().map_err(|e| err(ErrorCode::Failed, e.to_string()))?;
        let args = HelperArgs { pipe: server.name().to_string(), parent: std::process::id(), nonce };
        let child = launch_elevated(&self.exe, &args.to_command_line())?;
        self.finish_arm(server, child, &nonce)
    }

    /// Accept the connection from `child` and run the handshake. Split out so tests can
    /// drive it with an in-process fake helper.
    fn finish_arm(&mut self, mut server: PipeServer, child: OwnedHandle, nonce: &Nonce) -> Result<(), AdminError> {
        let pid = unsafe { GetProcessId(sys::h(&child)) };
        if pid == 0 {
            return Err(err(ErrorCode::Failed, "could not read the helper's process id"));
        }
        server.accept(CONNECT_TIMEOUT, Some(sys::h(&child))).map_err(|e| {
            let code = sys::exit_code(&child).map(|c| format!(" (helper exit code {c})")).unwrap_or_default();
            err(ErrorCode::Handshake, format!("the helper did not connect: {e}{code}"))
        })?;
        server
            .handshake(pid, nonce, HANDSHAKE_TIMEOUT)
            .map_err(|e| err(ErrorCode::Handshake, format!("helper handshake failed: {e}")))?;
        self.conn = Some(Conn { server, child });
        Ok(())
    }

    /// Armed and the helper process is still alive.
    pub fn is_armed(&mut self) -> bool {
        let alive = self.conn.as_ref().is_some_and(|c| !sys::has_exited(&c.child));
        if !alive {
            self.conn = None;
        }
        alive
    }

    /// Ask the helper to exit and close the pipe. Safe to call when not armed.
    pub fn disarm(&mut self) {
        if let Some(mut c) = self.conn.take() {
            let id = self.next_id;
            self.next_id += 1;
            let line = request_line(&Request { id, op: Op::Shutdown });
            if c.server.write_all(line.as_bytes(), Duration::from_secs(2)).is_ok() {
                let _ = c.server.read_line(MAX_RESPONSE_LINE, Duration::from_secs(2));
            }
            drop(c.server); // closing the pipe also makes the helper exit
            let _ = sys::wait_exit(&c.child, 3_000);
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        self.disarm();
    }
}

impl AdminApi for Helper {
    fn call(&mut self, op: Op) -> Result<Reply, AdminError> {
        if !self.is_armed() {
            return Err(err(ErrorCode::NotArmed, "the admin helper is not running"));
        }
        let id = self.next_id;
        self.next_id += 1;
        let timeout = reply_timeout(&op);
        let line = request_line(&Request { id, op });
        let conn = self.conn.as_mut().expect("armed");
        let result = conn
            .server
            .write_all(line.as_bytes(), WRITE_TIMEOUT)
            .and_then(|_| conn.server.read_line(MAX_RESPONSE_LINE, timeout));
        let raw = match result {
            Ok(r) => r,
            Err(e) => {
                // A broken or stuck channel is never reused.
                self.conn = None;
                return Err(err(ErrorCode::Transport, format!("admin helper channel failed: {e}")));
            }
        };
        let resp = match parse_response(&raw) {
            Ok(r) if r.id == id => r,
            Ok(_) => {
                self.conn = None;
                return Err(err(ErrorCode::Transport, "response id mismatch"));
            }
            Err(e) => {
                self.conn = None;
                return Err(e);
            }
        };
        resp.into_result()
    }
}

/// ShellExecuteEx "runas": the one UAC prompt. Returns the helper's process handle.
fn launch_elevated(exe: &Path, params: &str) -> Result<OwnedHandle, AdminError> {
    // ShellExecuteEx wants COM; initialise an STA if this thread has none yet.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.is_ok();
    let verb = HSTRING::from("runas");
    let file = HSTRING::from(exe.as_os_str());
    let params = HSTRING::from(params);
    let dir = HSTRING::from(exe.parent().map(|d| d.as_os_str()).unwrap_or_default());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        hwnd: HWND::default(),
        lpVerb: windows::core::PCWSTR(verb.as_ptr()),
        lpFile: windows::core::PCWSTR(file.as_ptr()),
        lpParameters: windows::core::PCWSTR(params.as_ptr()),
        lpDirectory: windows::core::PCWSTR(dir.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    let r = unsafe { ShellExecuteExW(&mut info) };
    if com {
        unsafe { CoUninitialize() };
    }
    match r {
        Ok(()) => sys::own(info.hProcess).ok_or_else(|| err(ErrorCode::Failed, "the helper started but no process handle came back")),
        Err(e) if e.code() == ERROR_CANCELLED.to_hresult() => {
            Err(err(ErrorCode::UacCancelled, "the administrator prompt was cancelled"))
        }
        Err(e) => Err(err(ErrorCode::Failed, format!("could not start the admin helper: {e}"))),
    }
}

// The app keeps one Helper behind a mutex and uses it from worker threads.
const _: fn() = || {
    fn is_send<T: Send>() {}
    is_send::<Helper>();
};

#[cfg(test)]
mod tests {
    use super::super::fake::{FakeBackend, FakeService};
    use super::super::handler::Policy;
    use super::super::pipe::{connect_client, helper_handshake, serve, LineReader};
    use super::*;

    fn fake() -> FakeBackend {
        let mut b = FakeBackend::default();
        b.add(FakeService::running("SysMain", StartType::Auto));
        b.add(FakeService::running("RpcSs", StartType::Auto));
        b
    }

    #[test]
    fn not_armed_until_armed() {
        let mut h = Helper::with_exe(r"C:\definitely\not\here\holdmyram-helper.exe");
        assert!(!h.is_armed());
        assert_eq!(h.ping().unwrap_err().code, ErrorCode::NotArmed);
        // A missing helper fails before any UAC prompt could appear.
        assert_eq!(h.arm().unwrap_err().code, ErrorCode::Failed);
        h.disarm();
        assert!(Helper::new().exe().ends_with(HELPER_EXE));
    }

    /// The real client code over a real pipe, with this test process standing in for the
    /// helper (no UAC, fake backend).
    #[test]
    fn typed_calls_over_the_pipe() {
        let server = PipeServer::create().unwrap();
        let name = server.name().to_string();
        let nonce = pipe::new_nonce().unwrap();
        let me = std::process::id();
        let helper = std::thread::spawn(move || {
            let file = connect_client(&name).unwrap();
            let mut reader = LineReader::new(&file, MAX_REQUEST_LINE);
            let mut writer = &file;
            helper_handshake(&mut reader, &mut writer, &nonce, me).unwrap();
            let mut handler = Handler::new(fake(), Policy::new(vec![me]));
            serve(&mut handler, &mut reader, &mut writer).unwrap();
            handler.backend().mutations()
        });
        let mut h = Helper::with_exe("unused");
        h.finish_arm(server, sys::open_process(me).unwrap(), &nonce).unwrap();
        assert!(h.is_armed());
        assert_eq!(h.ping().unwrap(), PROTOCOL_VERSION);
        let s = h.service_query("sysmain").unwrap();
        assert!(s.running && !s.protected);
        let rep = h.service_stop("SysMain").unwrap();
        assert_eq!(rep.stopped, vec!["SysMain"]);
        assert_eq!(h.service_stop("RpcSs").unwrap_err().code, ErrorCode::NeverTouch);
        let st = h.service_start("SysMain").unwrap();
        assert!(st.started);
        let t = h.service_set_start_type("SysMain", StartChoice::Manual).unwrap();
        assert_eq!((t.before.start, t.after.start), (StartType::Auto, StartType::Manual));
        assert_eq!(h.task_query(r"\Nope").unwrap_err().code, ErrorCode::NotFound);
        // Still armed after error replies.
        assert!(h.is_armed());
        h.disarm();
        assert!(!h.is_armed());
        assert_eq!(h.ping().unwrap_err().code, ErrorCode::NotArmed);
        assert_eq!(helper.join().unwrap(), vec!["stop SysMain", "start SysMain", "config SysMain Manual"]);
    }

    #[test]
    fn handshake_with_wrong_process_fails_and_leaves_disarmed() {
        let server = PipeServer::create().unwrap();
        let name = server.name().to_string();
        let t = std::thread::spawn(move || {
            let _f = connect_client(&name).unwrap();
            std::thread::sleep(Duration::from_millis(500));
        });
        // "Launched" child is some other process (explorer or this test's parent):
        // the connecting client (this process) does not match it.
        let other = std::process::id();
        let mut h = Helper::with_exe("unused");
        // Use a process handle whose pid differs from ours: our parent if we can open it.
        let parent = crate::list_processes().into_iter().find(|p| p.pid == other).map(|p| p.parent).unwrap_or(4);
        let child = sys::open_process(parent).unwrap_or_else(|_| sys::open_process(other).unwrap());
        let e = h.finish_arm(server, child, &[0u8; 32]).unwrap_err();
        assert_eq!(e.code, ErrorCode::Handshake);
        assert!(!h.is_armed());
        t.join().unwrap();
    }

    /// MANUAL ONLY (shows a UAC prompt). Launches the real release helper from this test
    /// exe. Answer Yes: the helper must refuse (exit code 5) because its parent is not
    /// holdmyram.exe, before touching anything. Answer No: UacCancelled.
    /// `cargo test -p holdmyram-core manual_uac_launch -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn manual_uac_launch_refuses_non_app_parent() {
        let exe = Path::new(env!("CARGO_MANIFEST_DIR")).join(r"..\..\target\release").join(HELPER_EXE);
        let mut h = Helper::with_exe(exe);
        let r = h.arm();
        eprintln!("arm() -> {r:?}");
        let e = r.unwrap_err();
        assert!(
            e.code == ErrorCode::UacCancelled || (e.code == ErrorCode::Handshake && e.message.contains("exit code 5")),
            "{e}"
        );
        assert!(!h.is_armed());
    }

    #[test]
    fn local_api_uses_the_same_wire_format() {
        let mut api = LocalApi::new(Handler::new(fake(), Policy::new(vec![])));
        assert_eq!(api.ping().unwrap(), PROTOCOL_VERSION);
        assert_eq!(api.service_stop("SysMain").unwrap().stopped, vec!["SysMain"]);
        assert_eq!(api.service_stop("RpcSs").unwrap_err().code, ErrorCode::NeverTouch);
        assert_eq!(api.service_stop("a/b").unwrap_err().code, ErrorCode::BadRequest);
    }
}
