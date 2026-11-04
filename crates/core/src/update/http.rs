//! The network layer: a tiny trait (tests use a fake) and its WinHTTP implementation.
//!
//! WinHTTP is part of Windows: system certificate store, system proxy settings, TLS by
//! the OS, no new crate. Every handle is opened per request and closed before returning,
//! so nothing network-related exists between checks. Redirects are never followed here;
//! `update::fetch` follows them itself so each hop is checked against the allow-list.

use super::Url;
use windows::core::PCWSTR;
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData,
    WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts, ERROR_WINHTTP_CANNOT_CONNECT, ERROR_WINHTTP_CONNECTION_ERROR,
    ERROR_WINHTTP_NAME_NOT_RESOLVED, ERROR_WINHTTP_TIMEOUT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_DISABLE_COOKIES, WINHTTP_FLAG_SECURE,
    WINHTTP_OPEN_REQUEST_FLAGS, WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_RAW_HEADERS_CRLF, WINHTTP_QUERY_STATUS_CODE,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// Lowercase names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetError {
    /// No connection at all (DNS, connect, timeout): reads as "offline".
    pub offline: bool,
    pub msg: String,
}

pub trait Http: Send + Sync {
    /// One GET without following redirects. The body is capped at `max` bytes (more is an
    /// error). `progress(received, content_length)` is called while reading.
    fn get(&self, url: &Url, accept: &str, max: u64, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<HttpResponse, NetError>;
}

/// Timeout for each step (resolve, connect, send, each receive), in milliseconds.
pub const TIMEOUT_MS: i32 = 5_000;
pub const USER_AGENT: &str = "HoldMyRAM";

pub struct WinHttp;

/// Owns one WinHTTP handle and closes it on drop, on every path (`?`, early return,
/// panic unwinding). Declared session -> connect -> request, so Rust drops (closes) them
/// request -> connect -> session. `close` is injectable for tests.
pub(crate) struct Guard {
    h: *mut core::ffi::c_void,
    close: fn(*mut core::ffi::c_void),
}

impl Guard {
    pub(crate) fn new(h: *mut core::ffi::c_void, close: fn(*mut core::ffi::c_void)) -> Guard {
        Guard { h, close }
    }
    fn get(&self) -> *mut core::ffi::c_void {
        self.h
    }
    fn is_null(&self) -> bool {
        self.h.is_null()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.h.is_null() {
            (self.close)(self.h);
            self.h = std::ptr::null_mut();
        }
    }
}

fn winhttp_close(h: *mut core::ffi::c_void) {
    unsafe {
        let _ = WinHttpCloseHandle(h);
    }
}

fn h(p: *mut core::ffi::c_void) -> Guard {
    Guard::new(p, winhttp_close)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn net_err(what: &str, e: windows::core::Error) -> NetError {
    let code = (e.code().0 as u32) & 0xFFFF;
    let offline = [ERROR_WINHTTP_NAME_NOT_RESOLVED, ERROR_WINHTTP_CANNOT_CONNECT, ERROR_WINHTTP_TIMEOUT, ERROR_WINHTTP_CONNECTION_ERROR].contains(&code);
    let msg = if offline { format!("{what}: no connection (WinHTTP error {code})") } else { format!("{what}: WinHTTP error {code}") };
    NetError { offline, msg }
}

fn last(what: &str) -> NetError {
    net_err(what, windows::core::Error::from_win32())
}

impl Http for WinHttp {
    fn get(&self, url: &Url, accept: &str, max: u64, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<HttpResponse, NetError> {
        unsafe {
            let ua = wide(USER_AGENT);
            let session = h(WinHttpOpen(PCWSTR(ua.as_ptr()), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0));
            if session.is_null() {
                return Err(last("opening WinHTTP"));
            }
            WinHttpSetTimeouts(session.get(), TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS).map_err(|e| net_err("timeouts", e))?;
            let host = wide(&url.host);
            let conn = h(WinHttpConnect(session.get(), PCWSTR(host.as_ptr()), url.port, 0));
            if conn.is_null() {
                return Err(last("connecting"));
            }
            let verb = wide("GET");
            let path = wide(&url.path);
            let flags = if url.https { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) };
            let req = h(WinHttpOpenRequest(conn.get(), PCWSTR(verb.as_ptr()), PCWSTR(path.as_ptr()), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), flags));
            if req.is_null() {
                return Err(last("opening the request"));
            }
            let never = WINHTTP_OPTION_REDIRECT_POLICY_NEVER.to_ne_bytes();
            WinHttpSetOption(Some(req.get()), WINHTTP_OPTION_REDIRECT_POLICY, Some(&never)).map_err(|e| net_err("redirect policy", e))?;
            let nocookies = WINHTTP_DISABLE_COOKIES.to_ne_bytes();
            let _ = WinHttpSetOption(Some(req.get()), WINHTTP_OPTION_DISABLE_FEATURE, Some(&nocookies));
            let headers: Vec<u16> = format!("Accept: {accept}\r\nX-GitHub-Api-Version: 2022-11-28\r\n").encode_utf16().collect();
            WinHttpSendRequest(req.get(), Some(&headers), None, 0, 0, 0).map_err(|e| net_err("sending", e))?;
            WinHttpReceiveResponse(req.get(), std::ptr::null_mut()).map_err(|e| net_err("receiving", e))?;

            let mut status: u32 = 0;
            let mut len = 4u32;
            let mut idx = 0u32;
            WinHttpQueryHeaders(req.get(), WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, PCWSTR::null(), Some(&mut status as *mut u32 as *mut _), &mut len, &mut idx)
                .map_err(|e| net_err("status", e))?;
            let headers = raw_headers(req.get());
            let total = headers.iter().find(|(n, _)| n == "content-length").and_then(|(_, v)| v.parse::<u64>().ok());
            if total.is_some_and(|t| t > max) {
                return Err(NetError { offline: false, msg: format!("response is larger than {max} bytes") });
            }

            let mut body: Vec<u8> = Vec::new();
            loop {
                let mut avail = 0u32;
                WinHttpQueryDataAvailable(req.get(), &mut avail).map_err(|e| net_err("reading", e))?;
                if avail == 0 {
                    break;
                }
                if body.len() as u64 + avail as u64 > max {
                    return Err(NetError { offline: false, msg: format!("response is larger than {max} bytes") });
                }
                let start = body.len();
                body.resize(start + avail as usize, 0);
                let mut read = 0u32;
                WinHttpReadData(req.get(), body[start..].as_mut_ptr() as *mut _, avail, &mut read).map_err(|e| net_err("reading", e))?;
                body.truncate(start + read as usize);
                progress(body.len() as u64, total);
                if read == 0 {
                    break;
                }
            }
            Ok(HttpResponse { status: status as u16, headers, body })
        }
    }
}

/// All response headers, lowercase names. Empty on error.
unsafe fn raw_headers(req: *mut core::ffi::c_void) -> Vec<(String, String)> {
    let mut len = 0u32;
    let mut idx = 0u32;
    let _ = unsafe { WinHttpQueryHeaders(req, WINHTTP_QUERY_RAW_HEADERS_CRLF, PCWSTR::null(), None, &mut len, &mut idx) };
    if len == 0 || len > 256 * 1024 {
        return Vec::new();
    }
    let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
    let mut idx = 0u32;
    if unsafe { WinHttpQueryHeaders(req, WINHTTP_QUERY_RAW_HEADERS_CRLF, PCWSTR::null(), Some(buf.as_mut_ptr() as *mut _), &mut len, &mut idx) }.is_err() {
        return Vec::new();
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    parse_headers(&String::from_utf16_lossy(&buf[..end]))
}

/// `Name: value` lines after the status line.
pub fn parse_headers(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .skip(1)
        .filter_map(|l| l.split_once(':').map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string())))
        .filter(|(n, _)| !n.is_empty())
        .collect()
}
