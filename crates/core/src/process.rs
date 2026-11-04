//! Read-only process data. Nothing here opens a process with more than query rights and
//! nothing polls: every function reads on demand and returns.
//!
//! The process table comes from `NtQuerySystemInformation(SystemProcessInformation)`,
//! the same system-wide table Task Manager uses. Unlike opening each process, it needs no
//! process handle, so it gives a working set for SYSTEM-owned, protected and
//! other-session processes too, without elevation. If that call is unavailable or fails,
//! we fall back to Toolhelp plus per-process `OpenProcess` (the old path).

use serde::Serialize;
use std::ffi::c_void;
use std::mem::size_of;
use windows::core::{s, w, PCWSTR, PWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{GetLogicalDriveStringsW, QueryDosDeviceW};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

#[derive(Debug, Clone, Serialize)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
    pub working_set_bytes: u64,
}

/// Snapshot of running processes. Read on demand only; nothing polls.
pub fn list_processes() -> Vec<ProcInfo> {
    match nt::process_table() {
        Some(v) if !v.is_empty() => v,
        _ => list_processes_toolhelp(),
    }
}

/// Fallback: Toolhelp for pid/parent/name, `OpenProcess` per PID for the working set
/// (0 where access is denied).
fn list_processes_toolhelp() -> Vec<ProcInfo> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                out.push(ProcInfo {
                    pid: entry.th32ProcessID,
                    parent: entry.th32ParentProcessID,
                    name: String::from_utf16_lossy(&entry.szExeFile[..len]),
                    working_set_bytes: working_set(entry.th32ProcessID),
                });
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

pub(crate) fn working_set(pid: u32) -> u64 {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return 0;
        };
        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        let ok = GetProcessMemoryInfo(handle, &mut counters, counters.cb).is_ok();
        let _ = CloseHandle(handle);
        if ok {
            counters.WorkingSetSize as u64
        } else {
            0
        }
    }
}

/// PID of the process that owns the foreground window, if any.
pub fn foreground_pid() -> Option<u32> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        (pid != 0).then_some(pid)
    }
}

// ---------------------------------------------------------------------------------------
// Image paths
// ---------------------------------------------------------------------------------------

/// Resolves full image paths by PID. Build one per scan: it caches the NT-device to
/// drive-letter map and a reusable name buffer.
///
/// Order: `SystemProcessIdInformation` (no process handle needed, NT device path mapped
/// to a drive letter), then `QueryFullProcessImageNameW` (needs a query handle), then the
/// unmapped NT path if that is all we got. Returns `None` when nothing worked; the
/// caller decides whether to fall back to the exe name.
pub struct ImagePaths {
    /// (NT device, drive) pairs such as ("\Device\HarddiskVolume3", "C:").
    devices: Vec<(String, String)>,
    buf: Vec<u16>,
}

impl Default for ImagePaths {
    fn default() -> Self {
        Self::new()
    }
}

impl ImagePaths {
    pub fn new() -> Self {
        ImagePaths { devices: dos_devices(), buf: Vec::new() }
    }

    pub fn get(&mut self, pid: u32) -> Option<String> {
        if pid == 0 {
            return None;
        }
        let nt_path = nt::image_path_by_pid(pid, &mut self.buf).filter(|p| !p.is_empty());
        if let Some(p) = nt_path.as_deref().and_then(|p| nt_to_dos(p, &self.devices)) {
            return Some(p);
        }
        query_full_image_name(pid).or(nt_path)
    }
}

pub(crate) fn query_full_image_name(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

fn dos_devices() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut drives = [0u16; 512];
    let n = unsafe { GetLogicalDriveStringsW(Some(&mut drives)) } as usize;
    if n == 0 || n > drives.len() {
        return out;
    }
    // "C:\\\0D:\\\0\0": take each entry's first two chars ("C:").
    for entry in drives[..n].split(|&c| c == 0).filter(|e| e.len() >= 2) {
        let drive = String::from_utf16_lossy(&entry[..2]);
        let q: Vec<u16> = drive.encode_utf16().chain(std::iter::once(0)).collect();
        let mut target = [0u16; 1024];
        let len = unsafe { QueryDosDeviceW(PCWSTR(q.as_ptr()), Some(&mut target)) } as usize;
        if len == 0 || len > target.len() {
            continue;
        }
        // Multi-string; the first entry is the current mapping.
        let end = target[..len].iter().position(|&c| c == 0).unwrap_or(len);
        let device = String::from_utf16_lossy(&target[..end]);
        if !device.is_empty() {
            out.push((device, drive));
        }
    }
    out
}

/// "\Device\HarddiskVolume3\Windows\x.exe" -> "C:\Windows\x.exe". The device must be
/// followed by a backslash so HarddiskVolume1 never matches HarddiskVolume10.
fn nt_to_dos(path: &str, devices: &[(String, String)]) -> Option<String> {
    for (device, drive) in devices {
        if path.len() > device.len()
            && path.is_char_boundary(device.len())
            && path[..device.len()].eq_ignore_ascii_case(device)
            && path[device.len()..].starts_with('\\')
        {
            return Some(format!("{drive}{}", &path[device.len()..]));
        }
    }
    const MUP: &str = r"\Device\Mup\";
    if path.len() > MUP.len() && path.is_char_boundary(MUP.len()) && path[..MUP.len()].eq_ignore_ascii_case(MUP) {
        return Some(format!(r"\\{}", &path[MUP.len()..]));
    }
    None
}

// ---------------------------------------------------------------------------------------
// ntdll: NtQuerySystemInformation
// ---------------------------------------------------------------------------------------

mod nt {
    use super::*;

    const SYSTEM_PROCESS_INFORMATION: u32 = 5;
    const SYSTEM_PROCESS_ID_INFORMATION: u32 = 88;
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
    const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023_u32 as i32;

    /// Field offsets in the x64 `SYSTEM_PROCESS_INFORMATION` (fixed part is 0x100 bytes).
    const OFF_NEXT: usize = 0x00;
    const OFF_NAME_LEN: usize = 0x38; // ImageName.Length (bytes)
    const OFF_NAME_BUF: usize = 0x40; // ImageName.Buffer
    const OFF_PID: usize = 0x50; // UniqueProcessId
    const OFF_PARENT: usize = 0x58; // InheritedFromUniqueProcessId
    const OFF_WORKING_SET: usize = 0x90; // WorkingSetSize
    /// Bytes of an entry we must be able to read: up to and including WorkingSetSize.
    pub(super) const MIN_ENTRY: usize = OFF_WORKING_SET + 8;

    type NtQuerySystemInformationFn = unsafe extern "system" fn(u32, *mut c_void, u32, *mut u32) -> i32;

    fn query_fn() -> Option<NtQuerySystemInformationFn> {
        unsafe {
            let module = GetModuleHandleW(w!("ntdll.dll")).ok()?;
            let p = GetProcAddress(module, s!("NtQuerySystemInformation"))?;
            Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, NtQuerySystemInformationFn>(p))
        }
    }

    /// The system process table, or `None` if it could not be read (32-bit build, entry
    /// point missing, or the call kept failing).
    pub fn process_table() -> Option<Vec<ProcInfo>> {
        // Offsets above are for the 64-bit layout only.
        if size_of::<usize>() != 8 {
            return None;
        }
        let f = query_fn()?;
        // u64 backing keeps the entries 8-byte aligned, as ntdll expects.
        let mut buf: Vec<u64> = vec![0; 1024 * 1024 / 8];
        for _ in 0..8 {
            let bytes = buf.len() * 8;
            let mut needed = 0u32;
            let status = unsafe { f(SYSTEM_PROCESS_INFORMATION, buf.as_mut_ptr() as *mut c_void, bytes as u32, &mut needed) };
            if status >= 0 {
                let used = (needed as usize).min(bytes);
                let used = if used == 0 { bytes } else { used };
                let data = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, used) };
                return Some(parse_process_table(data));
            }
            if status != STATUS_INFO_LENGTH_MISMATCH && status != STATUS_BUFFER_TOO_SMALL {
                return None;
            }
            // Processes can start between calls: add headroom over what was asked for.
            let want = (needed as usize).max(bytes * 2) + 64 * 1024;
            if want > 64 * 1024 * 1024 {
                return None;
            }
            buf = vec![0; want / 8 + 1];
        }
        None
    }

    fn rd_u16(d: &[u8], at: usize) -> Option<u16> {
        d.get(at..at.checked_add(2)?).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn rd_u32(d: &[u8], at: usize) -> Option<u32> {
        d.get(at..at.checked_add(4)?).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }
    fn rd_u64(d: &[u8], at: usize) -> Option<u64> {
        d.get(at..at.checked_add(8)?).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    /// Parse the `SYSTEM_PROCESS_INFORMATION` chain in `data`. ImageName.Buffer is an
    /// absolute pointer into the same buffer, so it is translated back to an offset from
    /// `data`'s start and bounds-checked; a name outside the buffer reads as empty.
    /// Never reads past `data`: a truncated or garbage buffer yields the entries that
    /// were fully readable before the problem, never a panic.
    pub fn parse_process_table(data: &[u8]) -> Vec<ProcInfo> {
        let base = data.as_ptr() as usize;
        let mut out = Vec::new();
        let mut off = 0usize;
        loop {
            let Some(end) = off.checked_add(MIN_ENTRY) else { break };
            if end > data.len() {
                break;
            }
            let (Some(next), Some(name_len), Some(name_ptr), Some(pid), Some(parent), Some(ws)) = (
                rd_u32(data, off + OFF_NEXT),
                rd_u16(data, off + OFF_NAME_LEN),
                rd_u64(data, off + OFF_NAME_BUF),
                rd_u64(data, off + OFF_PID),
                rd_u64(data, off + OFF_PARENT),
                rd_u64(data, off + OFF_WORKING_SET),
            ) else {
                break;
            };
            let name = read_name(data, base, name_ptr, name_len as usize);
            let name = if pid == 0 && name.is_empty() { "[System Process]".to_string() } else { name };
            out.push(ProcInfo {
                // PIDs are 32-bit in practice; anything wider is garbage, so stop.
                pid: match u32::try_from(pid) {
                    Ok(p) => p,
                    Err(_) => break,
                },
                parent: u32::try_from(parent).unwrap_or(0),
                name,
                working_set_bytes: ws,
            });
            if next == 0 {
                break;
            }
            // The chain only moves forward, so this always terminates.
            match off.checked_add(next as usize) {
                Some(n) if n > off && n < data.len() => off = n,
                _ => break,
            }
        }
        out
    }

    fn read_name(data: &[u8], base: usize, ptr: u64, len: usize) -> String {
        if ptr == 0 || len == 0 || len % 2 != 0 {
            return String::new();
        }
        let Some(start) = (ptr as usize).checked_sub(base) else { return String::new() };
        let Some(bytes) = start.checked_add(len).and_then(|end| data.get(start..end)) else {
            return String::new();
        };
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    }

    /// x64 `SYSTEM_PROCESS_ID_INFORMATION`: { HANDLE ProcessId; UNICODE_STRING ImageName; }
    #[repr(C)]
    struct ProcessIdInfo {
        pid: usize,
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    /// NT image path ("\Device\HarddiskVolume3\...") for a PID without opening it.
    /// `buf` is reused across calls and grown on demand.
    pub fn image_path_by_pid(pid: u32, buf: &mut Vec<u16>) -> Option<String> {
        if size_of::<usize>() != 8 {
            return None;
        }
        let f = query_fn()?;
        if buf.len() < 520 {
            buf.resize(520, 0);
        }
        for _ in 0..3 {
            let cap_bytes = (buf.len() * 2).min(u16::MAX as usize & !1) as u16;
            let mut info = ProcessIdInfo { pid: pid as usize, length: 0, maximum_length: cap_bytes, buffer: buf.as_mut_ptr() };
            let status = unsafe {
                f(
                    SYSTEM_PROCESS_ID_INFORMATION,
                    &mut info as *mut _ as *mut c_void,
                    size_of::<ProcessIdInfo>() as u32,
                    std::ptr::null_mut(),
                )
            };
            if status >= 0 {
                let units = (info.length as usize / 2).min(buf.len());
                return Some(String::from_utf16_lossy(&buf[..units]));
            }
            if status != STATUS_INFO_LENGTH_MISMATCH && status != STATUS_BUFFER_TOO_SMALL {
                return None;
            }
            // The required size comes back in MaximumLength; never shrink.
            let want = (info.maximum_length as usize / 2 + 1).max(buf.len() * 2).min(u16::MAX as usize / 2);
            if want <= buf.len() {
                return None;
            }
            buf.resize(want, 0);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::nt::{parse_process_table, MIN_ENTRY};
    use super::*;

    const ENTRY: usize = 0x100; // fixed-part size; names follow it

    /// One synthetic entry: (pid, parent, ws, name). Names are written after the fixed
    /// part and ImageName.Buffer points at them (absolute address in `buf`).
    fn build(entries: &[(u64, u64, u64, &str)]) -> Vec<u8> {
        let sizes: Vec<usize> = entries.iter().map(|e| (ENTRY + e.3.len() * 2 + 7) & !7).collect();
        let total: usize = sizes.iter().sum();
        let mut buf = vec![0u8; total];
        let base = buf.as_ptr() as usize;
        let mut off = 0;
        for (i, (pid, parent, ws, name)) in entries.iter().enumerate() {
            let next = if i + 1 == entries.len() { 0 } else { sizes[i] as u32 };
            buf[off..off + 4].copy_from_slice(&next.to_le_bytes());
            let units: Vec<u16> = name.encode_utf16().collect();
            buf[off + 0x38..off + 0x3A].copy_from_slice(&((units.len() * 2) as u16).to_le_bytes());
            buf[off + 0x3A..off + 0x3C].copy_from_slice(&((units.len() * 2) as u16).to_le_bytes());
            let name_off = off + ENTRY;
            let ptr = if units.is_empty() { 0u64 } else { (base + name_off) as u64 };
            buf[off + 0x40..off + 0x48].copy_from_slice(&ptr.to_le_bytes());
            buf[off + 0x50..off + 0x58].copy_from_slice(&pid.to_le_bytes());
            buf[off + 0x58..off + 0x60].copy_from_slice(&parent.to_le_bytes());
            buf[off + 0x90..off + 0x98].copy_from_slice(&ws.to_le_bytes());
            for (j, u) in units.iter().enumerate() {
                buf[name_off + j * 2..name_off + j * 2 + 2].copy_from_slice(&u.to_le_bytes());
            }
            off += sizes[i];
        }
        buf
    }

    fn sample() -> Vec<u8> {
        build(&[
            (0, 0, 8192, ""),
            (4, 0, 1_000_000, "System"),
            (1234, 4, 55_000_000, "svchost.exe"),
            (5678, 1234, 123, "MsMpEng.exe"),
        ])
    }

    #[test]
    fn parses_full_chain() {
        let buf = sample();
        let v = parse_process_table(&buf);
        assert_eq!(v.len(), 4);
        assert_eq!(v[0].pid, 0);
        assert_eq!(v[0].name, "[System Process]");
        assert_eq!((v[1].pid, v[1].parent, v[1].name.as_str(), v[1].working_set_bytes), (4, 0, "System", 1_000_000));
        assert_eq!((v[2].pid, v[2].parent, v[2].name.as_str(), v[2].working_set_bytes), (1234, 4, "svchost.exe", 55_000_000));
        assert_eq!((v[3].pid, v[3].parent, v[3].name.as_str(), v[3].working_set_bytes), (5678, 1234, "MsMpEng.exe", 123));
    }

    #[test]
    fn single_entry_with_zero_next() {
        let buf = build(&[(42, 7, 99, "a.exe")]);
        let v = parse_process_table(&buf);
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].pid, v[0].parent, v[0].name.as_str(), v[0].working_set_bytes), (42, 7, "a.exe", 99));
    }

    #[test]
    fn truncated_buffers_never_panic() {
        let buf = sample();
        // Every prefix length: never panics, never returns more than the full parse, and
        // what it returns matches the full parse entry for entry.
        let full = parse_process_table(&buf);
        for cut in 0..=buf.len() {
            let v = parse_process_table(&buf[..cut]);
            assert!(v.len() <= full.len());
            for (a, b) in v.iter().zip(&full) {
                assert_eq!((a.pid, a.parent, a.working_set_bytes), (b.pid, b.parent, b.working_set_bytes));
                // A name cut off by the truncation reads as empty, never as garbage.
                assert!(a.name == b.name || a.name.is_empty() || a.pid == 0);
            }
        }
        assert!(parse_process_table(&buf[..MIN_ENTRY - 1]).is_empty());
        assert_eq!(parse_process_table(&buf[..MIN_ENTRY]).len(), 1);
        assert!(parse_process_table(&[]).is_empty());
    }

    #[test]
    fn garbage_buffers_never_panic() {
        // Bad NextEntryOffset values: past the end, pointing backwards is impossible
        // (u32 forward offset) but huge values and tiny values must still terminate.
        for next in [1u32, 7, 0x100, 0xFFFF_FFFF, 0x8000_0000] {
            let mut buf = sample();
            buf[0..4].copy_from_slice(&next.to_le_bytes());
            let v = parse_process_table(&buf);
            assert!(!v.is_empty());
            assert_eq!(v[0].pid, 0);
            assert!(v.len() <= buf.len());
        }
        // Name pointer outside the buffer and odd name length: empty name, no panic.
        let mut buf = build(&[(9, 1, 5, "x.exe")]);
        buf[0x40..0x48].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(parse_process_table(&buf)[0].name, "");
        let mut buf = build(&[(9, 1, 5, "x.exe")]);
        buf[0x38..0x3A].copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert_eq!(parse_process_table(&buf)[0].name, "");
        // PID wider than 32 bits is garbage: stop there.
        let buf = build(&[(1u64 << 40, 1, 5, "x.exe")]);
        assert!(parse_process_table(&buf).is_empty());
        // Pseudo-random bytes of many sizes.
        let mut seed = 0x1234_5678_9abc_def0u64;
        for len in (0..4096).step_by(37) {
            let mut b = vec![0u8; len];
            for x in b.iter_mut() {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *x = seed as u8;
            }
            let _ = parse_process_table(&b);
        }
    }

    #[test]
    fn nt_paths_map_to_drive_letters() {
        let dev = vec![
            (r"\Device\HarddiskVolume1".to_string(), "D:".to_string()),
            (r"\Device\HarddiskVolume10".to_string(), "C:".to_string()),
        ];
        assert_eq!(nt_to_dos(r"\Device\HarddiskVolume10\Windows\a.exe", &dev).as_deref(), Some(r"C:\Windows\a.exe"));
        assert_eq!(nt_to_dos(r"\device\harddiskvolume1\x\b.exe", &dev).as_deref(), Some(r"D:\x\b.exe"));
        assert_eq!(nt_to_dos(r"\Device\HarddiskVolume2\b.exe", &dev), None);
        assert_eq!(nt_to_dos(r"\Device\Mup\srv\share\c.exe", &dev).as_deref(), Some(r"\\srv\share\c.exe"));
        assert_eq!(nt_to_dos("", &dev), None);
    }

    /// The live table includes this test process with a real working set and path.
    #[test]
    fn live_table_has_self() {
        let me = std::process::id();
        let v = list_processes();
        let p = v.iter().find(|p| p.pid == me).expect("own pid in table");
        assert!(p.working_set_bytes > 0);
        let exe = std::env::current_exe().unwrap();
        assert!(p.name.eq_ignore_ascii_case(&exe.file_name().unwrap().to_string_lossy()));
        let path = ImagePaths::new().get(me).expect("own path");
        assert!(path.eq_ignore_ascii_case(&exe.to_string_lossy()), "{path} vs {}", exe.display());
    }
}
