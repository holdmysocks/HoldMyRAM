//! Small Win32 helpers shared by both ends of the admin channel: randomness, hashing,
//! token and process queries. No side effects beyond opening handles.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use windows::core::PWSTR;
use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCryptHash, BCRYPT_SHA256_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TokenUser, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

pub fn h(o: &OwnedHandle) -> HANDLE {
    HANDLE(o.as_raw_handle())
}

/// Take ownership of a raw handle (closed on drop). None for null/invalid handles.
pub fn own(handle: HANDLE) -> Option<OwnedHandle> {
    if handle.is_invalid() || handle.0.is_null() {
        None
    } else {
        Some(unsafe { OwnedHandle::from_raw_handle(handle.0) })
    }
}

fn io_err(e: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error((e.code().0 as u32 & 0xFFFF) as i32)
}

pub fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut buf = [0u8; N];
    let st = unsafe { BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if st.is_ok() {
        Ok(buf)
    } else {
        Err(io::Error::other(format!("BCryptGenRandom failed ({:#x})", st.0)))
    }
}

pub fn sha256(data: &[u8]) -> io::Result<[u8; 32]> {
    let mut out = [0u8; 32];
    let st = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) };
    if st.is_ok() {
        Ok(out)
    } else {
        Err(io::Error::other(format!("BCryptHash failed ({:#x})", st.0)))
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Strict lowercase-hex decode into exactly N bytes.
pub fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 || !s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
        return None;
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Constant-time comparison (length is not secret).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether this process runs with an elevated (full administrator) token.
pub fn is_elevated() -> bool {
    unsafe {
        let mut tok = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).is_err() {
            return false;
        }
        let Some(tok) = own(tok) else { return false };
        let mut el = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        GetTokenInformation(
            h(&tok),
            TokenElevation,
            Some(&mut el as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok()
            && el.TokenIsElevated != 0
    }
}

/// The current user's SID as a string ("S-1-5-21-...").
pub fn current_user_sid() -> io::Result<String> {
    unsafe {
        let mut tok = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).map_err(io_err)?;
        let tok = own(tok).ok_or_else(|| io::Error::other("no token"))?;
        let mut len = 0u32;
        let _ = GetTokenInformation(h(&tok), TokenUser, None, 0, &mut len);
        if len == 0 {
            return Err(io::Error::other("GetTokenInformation(TokenUser) gave no size"));
        }
        let mut buf: Vec<u64> = vec![0; (len as usize).div_ceil(8)];
        GetTokenInformation(h(&tok), TokenUser, Some(buf.as_mut_ptr() as *mut _), (buf.len() * 8) as u32, &mut len)
            .map_err(io_err)?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s).map_err(io_err)?;
        let out = s.to_string().map_err(|_| io::Error::other("SID is not UTF-16"));
        let _ = LocalFree(Some(HLOCAL(s.0 as _)));
        out
    }
}

/// Open a process for identity checks and waiting (no other rights).
pub fn open_process(pid: u32) -> io::Result<OwnedHandle> {
    let hd = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, false, pid) }.map_err(io_err)?;
    own(hd).ok_or_else(|| io::Error::other("OpenProcess returned no handle"))
}

/// Full Win32 image path of a process.
pub fn image_path(process: &OwnedHandle) -> io::Result<PathBuf> {
    let mut buf = vec![0u16; 32_768];
    let mut len = buf.len() as u32;
    unsafe { QueryFullProcessImageNameW(h(process), PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }.map_err(io_err)?;
    Ok(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
}

/// Whether the process has exited (non-blocking).
pub fn has_exited(process: &OwnedHandle) -> bool {
    let r = unsafe { WaitForSingleObject(h(process), 0) };
    r == WAIT_OBJECT_0
}

/// Exit code of a process that has exited; None while it runs or if unreadable.
pub fn exit_code(process: &OwnedHandle) -> Option<u32> {
    if !has_exited(process) {
        return None;
    }
    let mut code = 0u32;
    unsafe { windows::Win32::System::Threading::GetExitCodeProcess(h(process), &mut code) }.ok()?;
    Some(code)
}

/// Wait for the process to exit, up to `ms` (u32::MAX = forever). True if it exited.
pub fn wait_exit(process: &OwnedHandle, ms: u32) -> bool {
    let r = unsafe { WaitForSingleObject(h(process), ms) };
    r != WAIT_TIMEOUT
}

/// Two paths name the same file: both canonicalised (symlinks, 8.3 names and `..`
/// resolved), then compared case-insensitively. Any error means "not the same".
pub fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x.to_string_lossy().to_lowercase() == y.to_string_lossy().to_lowercase(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_and_hash() {
        let a = random_bytes::<32>().unwrap();
        let b = random_bytes::<32>().unwrap();
        assert_ne!(a, b);
        // SHA-256("abc"), FIPS 180-2 test vector.
        assert_eq!(hex(&sha256(b"abc").unwrap()), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn hex_round_trip_is_strict() {
        let b = [0u8, 1, 0xab, 0xff];
        assert_eq!(hex(&b), "0001abff");
        assert_eq!(unhex::<4>("0001abff"), Some(b));
        assert_eq!(unhex::<4>("0001ABFF"), None, "uppercase rejected");
        assert_eq!(unhex::<4>("0001abf"), None);
        assert_eq!(unhex::<4>("0001abffaa"), None);
        assert_eq!(unhex::<4>("0001abfg"), None);
        assert_eq!(unhex::<1>("+f"), None);
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn own_process_queries() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        let me = open_process(std::process::id()).unwrap();
        assert!(!has_exited(&me));
        let p = image_path(&me).unwrap();
        assert!(same_file(&p, &std::env::current_exe().unwrap()));
        assert!(!same_file(&p, Path::new(r"C:\Windows\explorer.exe")));
        assert!(!same_file(&p, Path::new(r"C:\does\not\exist.exe")));
        // Tests run unelevated in normal use; just make sure the call works.
        let _ = is_elevated();
    }
}
