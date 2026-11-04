//! The channel between the app and the elevated helper.
//!
//! The NON-elevated app is the pipe SERVER: it creates `\\.\pipe\HoldMyRAM-admin-<32 hex>`
//! (128 random bits from BCryptGenRandom), one instance only, first-instance flag set,
//! remote clients rejected, DACL granting access to the current user's SID only. The
//! helper is the CLIENT and connects with identification-only impersonation, so the app
//! can never act with the helper's elevated token.
//!
//! Handshake, after the helper connects:
//! 1. App checks the client PID (GetNamedPipeClientProcessId) is the process it
//!    launched (from ShellExecuteEx's process handle).
//! 2. Helper checks the server PID (GetNamedPipeServerProcessId) is the `--parent` PID
//!    it was given, whose process handle it opened BEFORE connecting (so the PID cannot
//!    be recycled) and whose image is holdmyram.exe next to the helper.
//! 3. App sends `{"hello":"<nonce hex>","version":1}`; the helper compares it (constant
//!    time) with the nonce from its command line.
//! 4. Helper answers `{"proof":"<hex>","pid":<helper pid>,"version":1}` with
//!    proof = SHA-256("HoldMyRAM admin helper v1\0" | nonce | helper pid LE | app pid LE);
//!    the app recomputes and compares.
//!
//! Then request/response lines (proto.rs) until the pipe closes or Shutdown.

use super::handler::{AdminBackend, Handler};
use super::proto::{self, Hello, Proof, PROTOCOL_VERSION};
use super::sys::{self, h, own};
use std::io::{self, Read, Write};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::time::{Duration, Instant};

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, HLOCAL, LocalFree, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE,
    HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId, WaitNamedPipeW,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForMultipleObjects, WaitForSingleObject};

pub const PIPE_PREFIX: &str = r"\\.\pipe\HoldMyRAM-admin-";
const PROOF_DOMAIN: &[u8] = b"HoldMyRAM admin helper v1\0";

pub type Nonce = [u8; 32];

/// A fresh unpredictable pipe name.
pub fn new_pipe_name() -> io::Result<String> {
    Ok(format!("{PIPE_PREFIX}{}", sys::hex(&sys::random_bytes::<16>()?)))
}

/// The helper only ever connects to a local pipe of exactly our naming scheme.
pub fn is_valid_pipe_name(name: &str) -> bool {
    name.strip_prefix(PIPE_PREFIX).is_some_and(|rest| sys::unhex::<16>(rest).is_some())
}

pub fn new_nonce() -> io::Result<Nonce> {
    sys::random_bytes::<32>()
}

/// SHA-256(domain | nonce | helper pid LE | app pid LE).
pub fn proof(nonce: &Nonce, helper_pid: u32, app_pid: u32) -> io::Result<[u8; 32]> {
    let mut data = Vec::with_capacity(PROOF_DOMAIN.len() + 40);
    data.extend_from_slice(PROOF_DOMAIN);
    data.extend_from_slice(nonce);
    data.extend_from_slice(&helper_pid.to_le_bytes());
    data.extend_from_slice(&app_pid.to_le_bytes());
    sys::sha256(&data)
}

// ---------------------------------------------------------------------------------------
// Line framing
// ---------------------------------------------------------------------------------------

/// Reads `\n`-terminated lines with a hard length limit. A line over the limit is an
/// error (InvalidData) and the connection must be dropped; nothing after it is trusted.
pub struct LineReader<R> {
    inner: R,
    buf: Vec<u8>,
    max: usize,
}

impl<R: Read> LineReader<R> {
    pub fn new(inner: R, max: usize) -> Self {
        LineReader { inner, buf: Vec::new(), max }
    }

    /// Next line without its `\n` (and without a trailing `\r`). None at clean EOF.
    pub fn read_line(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=i).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.len() > self.max {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
                }
                return Ok(Some(line));
            }
            if self.buf.len() > self.max {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
            }
            let mut chunk = [0u8; 4096];
            let n = match self.inner.read(&mut chunk) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => 0,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if n == 0 {
                return if self.buf.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed mid-line"))
                };
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

// ---------------------------------------------------------------------------------------
// App side: the pipe server
// ---------------------------------------------------------------------------------------

/// SDDL for the pipe: protected DACL, one ACE, full access for this user only.
pub fn pipe_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GA;;;{user_sid})")
}

pub struct PipeServer {
    pipe: OwnedHandle,
    event: OwnedHandle,
    name: String,
    rbuf: Vec<u8>,
}

impl PipeServer {
    /// A new pipe with a random name, restricted to the current user.
    pub fn create() -> io::Result<PipeServer> {
        Self::create_named(&new_pipe_name()?)
    }

    pub fn create_named(name: &str) -> io::Result<PipeServer> {
        let sddl = HSTRING::from(pipe_sddl(&sys::current_user_sid()?));
        let mut psd = PSECURITY_DESCRIPTOR::default();
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut psd, None) }
            .map_err(|e| io::Error::other(format!("building the pipe DACL failed: {e}")))?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: false.into(),
        };
        let wname = HSTRING::from(name);
        let handle = unsafe {
            CreateNamedPipeW(
                &wname,
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                64 * 1024,
                64 * 1024,
                0,
                Some(&sa),
            )
        };
        let last = unsafe { GetLastError() };
        unsafe {
            let _ = LocalFree(Some(HLOCAL(psd.0)));
        }
        let pipe = own(handle).ok_or_else(|| io::Error::from_raw_os_error(last.0 as i32))?;
        let ev = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(|e| io::Error::other(e.to_string()))?;
        let event = own(ev).ok_or_else(|| io::Error::other("CreateEvent failed"))?;
        Ok(PipeServer { pipe, event, name: name.to_string(), rbuf: Vec::new() })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn handle(&self) -> HANDLE {
        h(&self.pipe)
    }

    /// Run one overlapped operation to completion or timeout. If `abort` is signalled
    /// first, the operation is cancelled and ConnectionAborted returned.
    fn overlapped(
        &self,
        timeout: Duration,
        abort: Option<HANDLE>,
        start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> io::Result<u32> {
        let event = h(&self.event);
        let mut ov = OVERLAPPED { hEvent: event, ..Default::default() };
        unsafe { ResetEvent(event) }.map_err(|e| io::Error::other(e.to_string()))?;
        match start(&mut ov) {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {}
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => return Ok(0),
            Err(e) => return Err(io::Error::from_raw_os_error((e.code().0 as u32 & 0xFFFF) as i32)),
        }
        let ms = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
        let r = match abort {
            Some(a) => unsafe { WaitForMultipleObjects(&[event, a], false, ms) },
            None => unsafe { WaitForSingleObject(event, ms) },
        };
        let mut n = 0u32;
        if r != WAIT_OBJECT_0 {
            // Timed out, aborted or wait failed: cancel and wait for the cancel so `ov`
            // is no longer in use when this frame returns.
            unsafe {
                let _ = CancelIoEx(self.handle(), Some(&ov));
                let _ = GetOverlappedResult(self.handle(), &ov, &mut n, true);
            }
            return Err(if r == WAIT_TIMEOUT {
                io::Error::new(io::ErrorKind::TimedOut, "timed out")
            } else {
                io::Error::new(io::ErrorKind::ConnectionAborted, "peer process exited")
            });
        }
        unsafe { GetOverlappedResult(self.handle(), &ov, &mut n, false) }
            .map_err(|e| io::Error::from_raw_os_error((e.code().0 as u32 & 0xFFFF) as i32))?;
        Ok(n)
    }

    /// Wait for the helper to connect. `abort` is the helper's process handle: if it
    /// exits first, give up at once.
    pub fn accept(&self, timeout: Duration, abort: Option<HANDLE>) -> io::Result<()> {
        let pipe = self.handle();
        self.overlapped(timeout, abort, |ov| unsafe { ConnectNamedPipe(pipe, Some(ov)) }).map(|_| ())
    }

    pub fn client_pid(&self) -> io::Result<u32> {
        let mut pid = 0u32;
        unsafe { GetNamedPipeClientProcessId(self.handle(), &mut pid) }.map_err(|e| io::Error::other(e.to_string()))?;
        Ok(pid)
    }

    pub fn write_all(&mut self, data: &[u8], timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut off = 0;
        while off < data.len() {
            let left = deadline.saturating_duration_since(Instant::now());
            let pipe = self.handle();
            let chunk = &data[off..];
            let n = self.overlapped(left, None, |ov| unsafe { windows::Win32::Storage::FileSystem::WriteFile(pipe, Some(chunk), None, Some(ov)) })?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "pipe accepted no data"));
            }
            off += n as usize;
        }
        Ok(())
    }

    /// One line (without `\n`), at most `max` bytes, within `timeout`.
    pub fn read_line(&mut self, max: usize, timeout: Duration) -> io::Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(i) = self.rbuf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.rbuf.drain(..=i).collect();
                line.pop();
                if line.len() > max {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
                }
                return Ok(line);
            }
            if self.rbuf.len() > max {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
            }
            let mut chunk = vec![0u8; 16 * 1024];
            let pipe = self.handle();
            let n = {
                let buf = &mut chunk[..];
                self.overlapped(left, None, |ov| unsafe {
                    windows::Win32::Storage::FileSystem::ReadFile(pipe, Some(buf), None, Some(ov))
                })?
            };
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "helper closed the pipe"));
            }
            self.rbuf.extend_from_slice(&chunk[..n as usize]);
        }
    }

    /// App side of the handshake (steps 1, 3 and 4 in the module docs).
    pub fn handshake(&mut self, expected_client_pid: u32, nonce: &Nonce, timeout: Duration) -> io::Result<()> {
        let fail = |m: String| io::Error::new(io::ErrorKind::PermissionDenied, m);
        let client = self.client_pid()?;
        if client != expected_client_pid {
            return Err(fail(format!("pipe client is pid {client}, expected the helper pid {expected_client_pid}")));
        }
        let hello = Hello { hello: sys::hex(nonce), version: PROTOCOL_VERSION };
        let mut line = serde_json::to_string(&hello).map_err(io::Error::other)?;
        line.push('\n');
        self.write_all(line.as_bytes(), timeout)?;
        let reply = self.read_line(proto::MAX_HANDSHAKE_LINE, timeout)?;
        let p: Proof = serde_json::from_slice(&reply).map_err(|e| fail(format!("bad handshake reply: {e}")))?;
        if p.version != PROTOCOL_VERSION {
            return Err(fail(format!("helper speaks protocol {}, expected {PROTOCOL_VERSION}", p.version)));
        }
        if p.pid != expected_client_pid {
            return Err(fail("helper reported a different pid".into()));
        }
        let want = proof(nonce, expected_client_pid, std::process::id())?;
        let got = sys::unhex::<32>(&p.proof).ok_or_else(|| fail("malformed proof".into()))?;
        if !sys::ct_eq(&want, &got) {
            return Err(fail("helper proof does not match".into()));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Helper side: the pipe client
// ---------------------------------------------------------------------------------------

/// Connect to the app's pipe as a client, identification-level impersonation only.
pub fn connect_client(name: &str) -> io::Result<std::fs::File> {
    if !is_valid_pipe_name(name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a HoldMyRAM pipe name"));
    }
    let wname = HSTRING::from(name);
    for _ in 0..2 {
        let r = unsafe {
            CreateFileW(
                &wname,
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                None,
            )
        };
        match r {
            Ok(hd) => return Ok(unsafe { std::fs::File::from_raw_handle(hd.0) }),
            Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() => unsafe {
                let _ = WaitNamedPipeW(&wname, 5_000);
            },
            Err(e) => return Err(io::Error::from_raw_os_error((e.code().0 as u32 & 0xFFFF) as i32)),
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "pipe stayed busy"))
}

pub fn server_pid(pipe: &std::fs::File) -> io::Result<u32> {
    use std::os::windows::io::AsRawHandle;
    let mut pid = 0u32;
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut pid) }.map_err(|e| io::Error::other(e.to_string()))?;
    Ok(pid)
}

/// Helper side of steps 3 and 4: read Hello, check the nonce, send the proof.
pub fn helper_handshake<R: Read, W: Write>(
    reader: &mut LineReader<R>,
    writer: &mut W,
    nonce: &Nonce,
    app_pid: u32,
) -> io::Result<()> {
    let fail = |m: &str| io::Error::new(io::ErrorKind::PermissionDenied, m.to_string());
    let line = reader.read_line()?.ok_or_else(|| fail("pipe closed before the handshake"))?;
    if line.len() > proto::MAX_HANDSHAKE_LINE {
        return Err(fail("handshake line too long"));
    }
    let hello: Hello = serde_json::from_slice(&line).map_err(|_| fail("malformed hello"))?;
    if hello.version != PROTOCOL_VERSION {
        return Err(fail("protocol version mismatch"));
    }
    let got = sys::unhex::<32>(&hello.hello).ok_or_else(|| fail("malformed nonce"))?;
    if !sys::ct_eq(&got, nonce) {
        return Err(fail("nonce mismatch"));
    }
    let me = std::process::id();
    let p = Proof { proof: sys::hex(&proof(nonce, me, app_pid)?), pid: me, version: PROTOCOL_VERSION };
    let mut out = serde_json::to_string(&p).map_err(io::Error::other)?;
    out.push('\n');
    writer.write_all(out.as_bytes())?;
    writer.flush()
}

/// Serve requests until EOF, Shutdown, or a protocol violation (oversized or
/// unterminated line: an error reply is sent, then the connection is dropped).
pub fn serve<B: AdminBackend, R: Read, W: Write>(
    handler: &mut Handler<B>,
    reader: &mut LineReader<R>,
    writer: &mut W,
) -> io::Result<()> {
    loop {
        let line = match reader.read_line() {
            Ok(Some(l)) => l,
            Ok(None) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let r = proto::Response::from_result(
                    0,
                    Err(proto::AdminError::new(proto::ErrorCode::BadRequest, "request line too long; closing")),
                );
                let _ = writer.write_all(r.to_line().as_bytes());
                let _ = writer.flush();
                return Err(e);
            }
            Err(e) => return Err(e),
        };
        let (resp, shutdown) = handler.handle_line(&line);
        writer.write_all(resp.to_line().as_bytes())?;
        writer.flush()?;
        if shutdown {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------------------
// Helper command line
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperArgs {
    pub pipe: String,
    pub parent: u32,
    pub nonce: Nonce,
}

impl HelperArgs {
    pub fn to_command_line(&self) -> String {
        format!("--pipe {} --parent {} --nonce {}", self.pipe, self.parent, sys::hex(&self.nonce))
    }

    /// Exactly `--pipe <name> --parent <pid> --nonce <64 hex>`, in any order, each once.
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<HelperArgs, String> {
        let args: Vec<String> = args.into_iter().collect();
        if args.len() != 6 {
            return Err("expected --pipe, --parent and --nonce".into());
        }
        let (mut pipe, mut parent, mut nonce) = (None, None, None);
        for pair in args.chunks(2) {
            let (k, v) = (pair[0].as_str(), pair[1].as_str());
            let slot_taken = match k {
                "--pipe" => pipe.replace(v.to_string()).is_some(),
                "--parent" => parent.replace(v.to_string()).is_some(),
                "--nonce" => nonce.replace(v.to_string()).is_some(),
                _ => return Err(format!("unknown argument {k}")),
            };
            if slot_taken {
                return Err(format!("{k} given twice"));
            }
        }
        let pipe = pipe.ok_or("missing --pipe")?;
        if !is_valid_pipe_name(&pipe) {
            return Err("bad pipe name".into());
        }
        let parent_s = parent.ok_or("missing --parent")?;
        let parent: u32 = parent_s
            .parse()
            .ok()
            .filter(|p| *p != 0 && parent_s.bytes().all(|c| c.is_ascii_digit()))
            .ok_or("bad parent pid")?;
        let nonce = sys::unhex::<32>(&nonce.ok_or("missing --nonce")?).ok_or("bad nonce")?;
        Ok(HelperArgs { pipe, parent, nonce })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::fake::{FakeBackend, FakeService};
    use super::super::handler::Policy;
    use super::super::proto::{ErrorCode, StartType};
    use super::*;

    #[test]
    fn pipe_names() {
        let a = new_pipe_name().unwrap();
        let b = new_pipe_name().unwrap();
        assert_ne!(a, b);
        assert!(is_valid_pipe_name(&a), "{a}");
        assert_eq!(a.len(), PIPE_PREFIX.len() + 32);
        for bad in [
            r"\\.\pipe\HoldMyRAM-admin-",
            r"\\.\pipe\HoldMyRAM-admin-0123456789abcdef0123456789ABCDEF",
            r"\\.\pipe\HoldMyRAM-admin-0123456789abcdef0123456789abcde",
            r"\\server\pipe\HoldMyRAM-admin-0123456789abcdef0123456789abcdef",
            r"\\.\pipe\Other-0123456789abcdef0123456789abcdef",
            r"C:\Windows\System32\config\SAM",
            r"\\.\pipe\HoldMyRAM-admin-0123456789abcdef0123456789abcdef\x",
        ] {
            assert!(!is_valid_pipe_name(bad), "{bad}");
        }
    }

    #[test]
    fn proof_binds_nonce_and_both_pids() {
        let n = [7u8; 32];
        let p = proof(&n, 10, 20).unwrap();
        assert_eq!(p, proof(&n, 10, 20).unwrap());
        assert_ne!(p, proof(&[8u8; 32], 10, 20).unwrap());
        assert_ne!(p, proof(&n, 11, 20).unwrap());
        assert_ne!(p, proof(&n, 10, 21).unwrap());
        assert_ne!(p, proof(&n, 20, 10).unwrap());
    }

    #[test]
    fn line_reader_limits() {
        let data = b"one\r\ntwo\n\nthree".to_vec();
        let mut r = LineReader::new(&data[..], 10);
        assert_eq!(r.read_line().unwrap().unwrap(), b"one");
        assert_eq!(r.read_line().unwrap().unwrap(), b"two");
        assert_eq!(r.read_line().unwrap().unwrap(), b"");
        assert_eq!(r.read_line().unwrap_err().kind(), io::ErrorKind::UnexpectedEof);

        let long = [b'x'; 11].iter().copied().chain(*b"\n").collect::<Vec<u8>>();
        let mut r = LineReader::new(&long[..], 10);
        assert_eq!(r.read_line().unwrap_err().kind(), io::ErrorKind::InvalidData);
        // Unterminated garbage over the limit is cut off without reading forever.
        let endless = vec![b'x'; 100_000];
        let mut r = LineReader::new(&endless[..], 4096);
        assert_eq!(r.read_line().unwrap_err().kind(), io::ErrorKind::InvalidData);
        let mut r = LineReader::new(&b""[..], 10);
        assert!(r.read_line().unwrap().is_none());
    }

    #[test]
    fn helper_args() {
        let n = [0xabu8; 32];
        let a = HelperArgs { pipe: format!("{PIPE_PREFIX}{}", "0".repeat(32)), parent: 1234, nonce: n };
        let cl = a.to_command_line();
        let parsed = HelperArgs::parse(cl.split(' ').map(String::from)).unwrap();
        assert_eq!(parsed, a);
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let pipe = a.pipe.as_str();
        let nh = sys::hex(&n);
        // Any order is fine.
        assert!(HelperArgs::parse(s(&["--nonce", &nh, "--pipe", pipe, "--parent", "1"])).is_ok());
        for bad in [
            s(&[]),
            s(&["--pipe", pipe, "--parent", "1234"]),
            s(&["--pipe", pipe, "--parent", "1234", "--nonce", &nh, "--x", "y"]),
            s(&["--pipe", pipe, "--pipe", pipe, "--nonce", &nh]),
            s(&["--pipe", r"\\.\pipe\evil", "--parent", "1234", "--nonce", &nh]),
            s(&["--pipe", pipe, "--parent", "0", "--nonce", &nh]),
            s(&["--pipe", pipe, "--parent", "+12", "--nonce", &nh]),
            s(&["--pipe", pipe, "--parent", "-1", "--nonce", &nh]),
            s(&["--pipe", pipe, "--parent", "99999999999", "--nonce", &nh]),
            s(&["--pipe", pipe, "--parent", "1234", "--nonce", &nh[..62]]),
            s(&["--pipe", pipe, "--parent", "1234", "--nonce", &nh.to_uppercase()]),
            s(&["--Pipe", pipe, "--parent", "1234", "--nonce", &nh]),
        ] {
            assert!(HelperArgs::parse(bad.clone()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn pipe_dacl_is_current_user_only() {
        use windows::core::PWSTR;
        use windows::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_KERNEL_OBJECT,
        };
        use windows::Win32::Security::DACL_SECURITY_INFORMATION;
        let server = PipeServer::create().unwrap();
        let sid = sys::current_user_sid().unwrap();
        let mut psd = PSECURITY_DESCRIPTOR::default();
        let r = unsafe { GetSecurityInfo(server.handle(), SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION, None, None, None, None, Some(&mut psd)) };
        assert!(r.is_ok(), "{r:?}");
        let mut s = PWSTR::null();
        unsafe { ConvertSecurityDescriptorToStringSecurityDescriptorW(psd, SDDL_REVISION_1, DACL_SECURITY_INFORMATION, &mut s, None) }.unwrap();
        let sddl = unsafe { s.to_string() }.unwrap();
        unsafe {
            let _ = LocalFree(Some(HLOCAL(s.0 as _)));
            let _ = LocalFree(Some(HLOCAL(psd.0)));
        }
        assert!(sddl.starts_with("D:P"), "{sddl}");
        assert_eq!(sddl.matches("(A;").count(), 1, "exactly one allow ACE: {sddl}");
        assert!(sddl.contains(&sid), "{sddl}");
        // Same name twice: FILE_FLAG_FIRST_PIPE_INSTANCE refuses a second server.
        assert!(PipeServer::create_named(server.name()).is_err());
    }

    /// End-to-end over a real pipe: app-side server and handshake on this thread, a fake
    /// "helper" (same checks, fake backend) on another. No elevation, no real services.
    #[test]
    fn transport_round_trip_with_fake_helper() {
        let mut server = PipeServer::create().unwrap();
        let name = server.name().to_string();
        let nonce = new_nonce().unwrap();
        let app_pid = std::process::id();
        let helper = std::thread::spawn(move || {
            let file = connect_client(&name).unwrap();
            assert_eq!(server_pid(&file).unwrap(), app_pid);
            let mut reader = LineReader::new(&file, proto::MAX_REQUEST_LINE);
            let mut writer = &file;
            helper_handshake(&mut reader, &mut writer, &nonce, app_pid).unwrap();
            let mut b = FakeBackend::default();
            b.add(FakeService::running("SysMain", StartType::Auto));
            let mut handler = Handler::new(b, Policy::new(vec![app_pid]));
            serve(&mut handler, &mut reader, &mut writer).unwrap();
            handler.backend().mutations()
        });
        server.accept(Duration::from_secs(10), None).unwrap();
        server.handshake(app_pid, &nonce, Duration::from_secs(10)).unwrap();
        let t = Duration::from_secs(10);
        server.write_all(br#"{"id":1,"op":{"ServiceStop":{"name":"SysMain"}}}"#, t).unwrap();
        server.write_all(b"\n", t).unwrap();
        let r = proto::parse_response(&server.read_line(proto::MAX_RESPONSE_LINE, t).unwrap()).unwrap();
        assert_eq!(r.id, 1);
        assert!(r.into_result().is_ok());
        server.write_all(b"{\"id\":2,\"op\":{\"ServiceStop\":{\"name\":\"RpcSs\"}}}\n", t).unwrap();
        let r = proto::parse_response(&server.read_line(proto::MAX_RESPONSE_LINE, t).unwrap()).unwrap();
        assert_eq!(r.into_result().unwrap_err().code, ErrorCode::NeverTouch);
        server.write_all(b"{\"id\":3,\"op\":\"Shutdown\"}\n", t).unwrap();
        let r = proto::parse_response(&server.read_line(proto::MAX_RESPONSE_LINE, t).unwrap()).unwrap();
        assert_eq!(r.id, 3);
        assert_eq!(helper.join().unwrap(), vec!["stop SysMain"]);
    }

    #[test]
    fn wrong_nonce_is_refused_by_the_helper() {
        let mut server = PipeServer::create().unwrap();
        let name = server.name().to_string();
        let helper_nonce = new_nonce().unwrap();
        let app_pid = std::process::id();
        let helper = std::thread::spawn(move || {
            let file = connect_client(&name).unwrap();
            let mut reader = LineReader::new(&file, proto::MAX_REQUEST_LINE);
            let mut writer = &file;
            helper_handshake(&mut reader, &mut writer, &helper_nonce, app_pid).unwrap_err()
            // file dropped: pipe closes
        });
        server.accept(Duration::from_secs(10), None).unwrap();
        let other = new_nonce().unwrap();
        let e = server.handshake(app_pid, &other, Duration::from_secs(10)).unwrap_err();
        assert!(matches!(e.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::PermissionDenied | io::ErrorKind::Other), "{e:?}");
        assert_eq!(helper.join().unwrap().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn app_refuses_unexpected_client_pid_and_bad_proof() {
        // Client pid check.
        let mut server = PipeServer::create().unwrap();
        let name = server.name().to_string();
        let t = std::thread::spawn(move || {
            let f = connect_client(&name).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            drop(f);
        });
        server.accept(Duration::from_secs(10), None).unwrap();
        let e = server.handshake(std::process::id() + 1, &[1u8; 32], Duration::from_secs(5)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        t.join().unwrap();

        // A client that knows the nonce but answers with a wrong proof.
        let mut server = PipeServer::create().unwrap();
        let name = server.name().to_string();
        let t = std::thread::spawn(move || {
            let file = connect_client(&name).unwrap();
            let mut reader = LineReader::new(&file, 512);
            let _hello = reader.read_line().unwrap();
            let mut w = &file;
            let p = Proof { proof: "00".repeat(32), pid: std::process::id(), version: PROTOCOL_VERSION };
            w.write_all(format!("{}\n", serde_json::to_string(&p).unwrap()).as_bytes()).unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });
        server.accept(Duration::from_secs(10), None).unwrap();
        let e = server.handshake(std::process::id(), &[1u8; 32], Duration::from_secs(5)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
        t.join().unwrap();
    }

    #[test]
    fn accept_times_out_and_oversized_requests_drop_the_connection() {
        let server = PipeServer::create().unwrap();
        let e = server.accept(Duration::from_millis(200), None).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);

        let mut server = PipeServer::create().unwrap();
        let name = server.name().to_string();
        let helper = std::thread::spawn(move || {
            let file = connect_client(&name).unwrap();
            let mut reader = LineReader::new(&file, proto::MAX_REQUEST_LINE);
            let mut writer = &file;
            let mut handler = Handler::new(FakeBackend::default(), Policy::new(vec![]));
            serve(&mut handler, &mut reader, &mut writer).unwrap_err().kind()
        });
        server.accept(Duration::from_secs(10), None).unwrap();
        let big = vec![b'x'; proto::MAX_REQUEST_LINE + 10];
        server.write_all(&big, Duration::from_secs(10)).unwrap();
        server.write_all(b"\n", Duration::from_secs(10)).unwrap();
        let r = proto::parse_response(&server.read_line(proto::MAX_RESPONSE_LINE, Duration::from_secs(10)).unwrap()).unwrap();
        assert_eq!(r.into_result().unwrap_err().code, ErrorCode::BadRequest);
        assert_eq!(helper.join().unwrap(), io::ErrorKind::InvalidData);
        // And the helper has hung up.
        assert!(server.read_line(100, Duration::from_secs(5)).is_err());
    }
}
