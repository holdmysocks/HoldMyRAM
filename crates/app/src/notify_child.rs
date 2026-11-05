//! The notification child: `holdmyram.exe --notify-toast <json>` / `--notify-popup <json>`,
//! and the sound-preview child `--notify-sound <json>`.
//! Runs before anything else in `main`, shows one notification (or plays one sound) and
//! exits. Everything here (WinRT, GDI, sound) loads only in this short-lived process.
//! Sound files come only from the fixed table in `notify::sound_for`, joined to
//! `%SystemRoot%\Media`; a missing file falls back to the system sound alias.
//!
//! Toast: an unpackaged app needs an AppUserModelID for Windows to show its name and
//! icon. The child registers one under
//! `HKCU\Software\Classes\AppUserModelId\HoldMyRAM` (values DisplayName and IconUri; the
//! icon PNG is written once to `%LOCALAPPDATA%\HoldMyRAM\notify-icon.png`). The toast is
//! silent (`<audio silent="true"/>`); a sound, if asked for, is played by this child.
//! On a toast error the pop-up is shown instead (exit code 10); notifications turned off
//! in Windows settings are respected (nothing shown, exit code 11).
//!
//! Pop-up: a small layered, click-through, never-activated, topmost window above the
//! taskbar for about 2 s. A newer pop-up replaces an older one (named mutex + WM_CLOSE to
//! the older window of this same exe), so toggles never stack them.

use crate::debug_log;
use crate::notify::{self, ChildMode, Note, NoteKind};

pub const AUMID: &str = "HoldMyRAM";
const ICON_PNG: &[u8] = include_bytes!("../icons/128x128.png");

/// Entry from `main`: returns the process exit code.
pub fn run(mode: ChildMode, note: &Note) -> i32 {
    match mode {
        ChildMode::Popup => popup_with_sound(note),
        ChildMode::Toast => match toast::show(note) {
            Ok(()) => {
                if note.sound {
                    sound::play(note.sound_set, note.kind, true);
                }
                notify::EXIT_OK
            }
            Err(toast::Error::Disabled(why)) => {
                debug_log(&format!("notify: toast not shown: {why}"));
                notify::EXIT_TOAST_DISABLED
            }
            Err(toast::Error::Failed(why)) => {
                debug_log(&format!("notify: toast failed ({why}); showing the pop-up instead"));
                match popup_with_sound(note) {
                    notify::EXIT_OK => notify::EXIT_TOAST_FELL_BACK,
                    other => other,
                }
            }
        },
    }
}

fn popup_with_sound(note: &Note) -> i32 {
    if note.sound {
        sound::play(note.sound_set, note.kind, false);
    }
    match popup::show(note) {
        Ok(()) => notify::EXIT_OK,
        Err(e) => {
            debug_log(&format!("notify: pop-up failed: {e}"));
            notify::EXIT_POPUP_FAILED
        }
    }
}

/// The sound-preview child: play one sound (synchronously) and exit.
pub fn run_sound(req: &notify::SoundReq) -> i32 {
    match sound::play(req.set, req.kind.note_kind(), true) {
        sound::Played::AsChosen => notify::EXIT_OK,
        sound::Played::FellBack => notify::EXIT_SOUND_FELL_BACK,
        sound::Played::Nothing => notify::EXIT_SOUND_FAILED,
    }
}

/// What to play for `set`/`kind`, given the Windows media folder (None: unknown) and a
/// file-exists check: the set's fixed file if it is there, else the system sound alias
/// for that kind (never silence because of a missing file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    File(std::path::PathBuf),
    Alias(&'static str),
}

pub fn resolve(
    set: holdmyram_core::config::NotifySoundSet,
    kind: NoteKind,
    media_dir: Option<&std::path::Path>,
    exists: impl Fn(&std::path::Path) -> bool,
) -> Resolved {
    match notify::sound_for(set, kind) {
        notify::SoundRef::Alias(a) => Resolved::Alias(a),
        notify::SoundRef::MediaFile(name) => match media_dir {
            Some(dir) => {
                let p = dir.join(name);
                if exists(&p) {
                    Resolved::File(p)
                } else {
                    Resolved::Alias(notify::alias_for(kind))
                }
            }
            None => Resolved::Alias(notify::alias_for(kind)),
        },
    }
}

mod sound {
    use super::{resolve, NoteKind, Resolved};
    use holdmyram_core::config::NotifySoundSet;
    use std::path::PathBuf;
    use windows::core::{s, w, BOOL, PCWSTR};
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Media::Audio::{SND_ALIAS, SND_ASYNC, SND_FILENAME, SND_FLAGS, SND_NODEFAULT, SND_SYNC};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;

    type PlaySoundFn = unsafe extern "system" fn(PCWSTR, HMODULE, u32) -> BOOL;

    /// PlaySoundW, looked up at run time from System32's winmm.dll so that the exe has no
    /// static winmm import and the tray process never maps it (only children call this).
    #[allow(non_snake_case)]
    unsafe fn PlaySoundW(sound: PCWSTR, _module: Option<HMODULE>, flags: SND_FLAGS) -> BOOL {
        let Ok(lib) = LoadLibraryExW(w!("winmm.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) else {
            crate::debug_log("notify: winmm.dll could not be loaded");
            return BOOL(0);
        };
        let Some(f) = GetProcAddress(lib, s!("PlaySoundW")) else {
            return BOOL(0);
        };
        let f: PlaySoundFn = std::mem::transmute(f);
        // The library stays loaded for the rest of this short-lived process (async play).
        f(sound, HMODULE::default(), flags.0)
    }

    pub enum Played {
        AsChosen,
        /// The set's file was missing or would not play; the system sound was tried.
        FellBack,
        Nothing,
    }

    /// `%SystemRoot%\Media` from the Windows directory API, else the SystemRoot variable.
    fn media_dir() -> Option<PathBuf> {
        let mut buf = [0u16; 512];
        let n = unsafe { GetSystemWindowsDirectoryW(Some(&mut buf)) } as usize;
        let win = if n > 0 && n < buf.len() {
            Some(PathBuf::from(String::from_utf16_lossy(&buf[..n])))
        } else {
            std::env::var_os("SystemRoot").map(PathBuf::from)
        };
        win.filter(|p| p.is_absolute()).map(|p| p.join("Media"))
    }

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn play_alias(alias: &str, mode: windows::Win32::Media::Audio::SND_FLAGS) -> bool {
        let a = wide(std::ffi::OsStr::new(alias));
        unsafe { PlaySoundW(PCWSTR(a.as_ptr()), None, SND_ALIAS | SND_NODEFAULT | mode) }.as_bool()
    }

    /// Play the set's sound for `kind` (warn: "Exclamation" for every set; the device
    /// set: "Device Connect"/"Device Disconnect" from the user's sound scheme). A missing
    /// or unplayable file falls back to the system sound. `wait`: block until it ends
    /// (the toast and preview children exit right after).
    pub fn play(set: NotifySoundSet, kind: NoteKind, wait: bool) -> Played {
        let mode = if wait { SND_SYNC } else { SND_ASYNC };
        let dir = media_dir();
        match resolve(set, kind, dir.as_deref(), |p| p.is_file()) {
            Resolved::File(path) => {
                let p = wide(path.as_os_str());
                // PlaySound copies the file name, so `p` may go away while it plays async.
                if unsafe { PlaySoundW(PCWSTR(p.as_ptr()), None, SND_FILENAME | SND_NODEFAULT | mode) }.as_bool() {
                    return Played::AsChosen;
                }
                crate::debug_log(&format!("notify: could not play {}; using the system sound", path.display()));
                if play_alias(crate::notify::alias_for(kind), mode) {
                    Played::FellBack
                } else {
                    Played::Nothing
                }
            }
            Resolved::Alias(a) => {
                let fell_back = !matches!(crate::notify::sound_for(set, kind), crate::notify::SoundRef::Alias(_));
                if fell_back {
                    crate::debug_log(&format!("notify: sound file for {} is missing; using the system sound", set.as_str()));
                }
                match (play_alias(a, mode), fell_back) {
                    (true, false) => Played::AsChosen,
                    (true, true) => Played::FellBack,
                    (false, _) => Played::Nothing,
                }
            }
        }
    }
}

/// Text for XML element content and attribute values.
pub fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

/// The toast XML: two text lines, silent, short.
pub fn toast_xml(note: &Note) -> String {
    format!(
        "<toast duration=\"short\"><visual><binding template=\"ToastGeneric\"><text hint-maxLines=\"1\">{}</text><text>{}</text></binding></visual><audio silent=\"true\"/></toast>",
        xml_escape(&note.title),
        xml_escape(&note.detail)
    )
}

mod toast {
    use super::{toast_xml, Note, AUMID, ICON_PNG};
    use windows::core::{Interface, HSTRING, PCWSTR};
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::{DateTime, IReference, PropertyValue};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegGetValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE,
        REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
    };
    use windows::UI::Notifications::{NotificationSetting, ToastNotification, ToastNotificationManager};

    pub enum Error {
        /// Turned off in Windows settings: respected, nothing else is shown.
        Disabled(String),
        Failed(String),
    }

    fn failed(what: &str) -> impl Fn(windows::core::Error) -> Error + '_ {
        move |e| Error::Failed(format!("{what}: {e}"))
    }

    pub fn show(note: &Note) -> Result<(), Error> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok().map_err(failed("COM"))?;
        if let Err(e) = register() {
            // Not fatal: Windows still shows the toast, with the id as its name.
            crate::debug_log(&format!("notify: AUMID registration: {e}"));
        }
        let doc = XmlDocument::new().map_err(failed("XmlDocument"))?;
        doc.LoadXml(&HSTRING::from(toast_xml(note))).map_err(failed("toast XML"))?;
        let toast = ToastNotification::CreateToastNotification(&doc).map_err(failed("toast"))?;
        // Same tag and group: a newer toast replaces the older one in the action center.
        let _ = toast.SetTag(&HSTRING::from("state"));
        let _ = toast.SetGroup(&HSTRING::from("holdmyram"));
        let _ = toast.SetExpiresOnReboot(true);
        if let Some(exp) = expires_in(15 * 60) {
            let _ = toast.SetExpirationTime(&exp);
        }
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID)).map_err(failed("notifier"))?;
        match notifier.Setting() {
            Ok(NotificationSetting::Enabled) | Err(_) => {}
            Ok(s) => return Err(Error::Disabled(format!("Windows notification setting {s:?}"))),
        }
        notifier.Show(&toast).map_err(failed("show"))
    }

    fn expires_in(secs: u64) -> Option<IReference<DateTime>> {
        let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() + secs;
        // 100 ns ticks since 1601-01-01.
        let ticks = (unix as i64 + 11_644_473_600) * 10_000_000;
        PropertyValue::CreateDateTime(DateTime { UniversalTime: ticks }).ok()?.cast().ok()
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn icon_path() -> Option<std::path::PathBuf> {
        let dir = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("HoldMyRAM");
        let p = dir.join("notify-icon.png");
        let same = std::fs::metadata(&p).map(|m| m.len() == ICON_PNG.len() as u64).unwrap_or(false);
        if !same {
            std::fs::create_dir_all(&dir).ok()?;
            let tmp = dir.join("notify-icon.png.tmp");
            std::fs::write(&tmp, ICON_PNG).ok()?;
            std::fs::rename(&tmp, &p).ok()?;
        }
        Some(p)
    }

    fn read_sz(key: HKEY, name: &str) -> Option<String> {
        let mut buf = vec![0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        let n = wide(name);
        let r = unsafe {
            RegGetValueW(key, PCWSTR::null(), PCWSTR(n.as_ptr()), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut len))
        };
        if r.is_err() {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }

    fn write_sz(key: HKEY, name: &str, value: &str) -> Result<(), String> {
        if read_sz(key, name).as_deref() == Some(value) {
            return Ok(());
        }
        let n = wide(name);
        let v = wide(value);
        let bytes = unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), v.len() * 2) };
        unsafe { RegSetValueExW(key, PCWSTR(n.as_ptr()), None, REG_SZ, Some(bytes)) }
            .ok()
            .map_err(|e| format!("{name}: {e}"))
    }

    /// One key, two values; rewritten only when different.
    fn register() -> Result<(), String> {
        let sub = wide(&format!(r"Software\Classes\AppUserModelId\{AUMID}"));
        let mut key = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(sub.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
        }
        .ok()
        .map_err(|e| format!("create key: {e}"))?;
        let mut r = write_sz(key, "DisplayName", "HoldMyRAM");
        if let Some(icon) = icon_path() {
            r = r.and(write_sz(key, "IconUri", &icon.display().to_string()));
        }
        unsafe {
            let _ = RegCloseKey(key);
        }
        r
    }
}

/// Pop-up opacity over time (ms since shown): fade in 120 ms, hold, fade out from 1700
/// to 2000 ms; None when it should close.
pub fn popup_alpha(ms: u64) -> Option<u8> {
    const MAX: u64 = 240;
    const IN: u64 = 120;
    const OUT_START: u64 = 1700;
    const END: u64 = 2000;
    match ms {
        m if m < IN => Some((MAX * (m + 1) / IN).min(MAX) as u8),
        m if m < OUT_START => Some(MAX as u8),
        m if m < END => Some((MAX * (END - m) / (END - OUT_START)) as u8),
        _ => None,
    }
}

mod popup {
    use super::{popup_alpha, Note, NoteKind};
    use std::sync::OnceLock;
    use std::time::Instant;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WAIT_TIMEOUT, WPARAM};
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::{
        CreateMutexW, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::HiDpi::{
        GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
    };
    use windows::Win32::UI::WindowsAndMessaging::*;

    const CLASS: PCWSTR = w!("HoldMyRAM.NotifyPopup");
    const TIMER_FADE: usize = 1;
    const TIMER_SAFETY: usize = 2;

    struct Layout {
        title: Vec<u16>,
        detail: Vec<u16>,
        dot: COLORREF,
        scale: f32,
        title_h: i32,
        detail_h: i32,
        w: i32,
    }
    static LAYOUT: OnceLock<Layout> = OnceLock::new();
    static START: OnceLock<Instant> = OnceLock::new();

    fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
        COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
    }

    fn px(scale: f32, v: i32) -> i32 {
        (v as f32 * scale).round() as i32
    }

    fn font(scale: f32, size: i32, weight: i32) -> HFONT {
        unsafe {
            CreateFontW(
                -px(scale, size),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                w!("Segoe UI"),
            )
        }
    }

    /// Height of `text` drawn wrapped in `width` with `f`, capped at `max_lines` lines.
    fn measure(text: &mut [u16], f: HFONT, width: i32, max_lines: i32) -> i32 {
        unsafe {
            let dc = GetDC(None);
            let old = SelectObject(dc, f.into());
            let mut tm = TEXTMETRICW::default();
            let _ = GetTextMetricsW(dc, &mut tm);
            let mut r = RECT { left: 0, top: 0, right: width, bottom: 0 };
            DrawTextW(dc, text, &mut r, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX);
            SelectObject(dc, old);
            ReleaseDC(None, dc);
            let line = tm.tmHeight.max(1);
            (r.bottom - r.top).clamp(line, line * max_lines)
        }
    }

    fn exe_name_of(pid: u32) -> Option<String> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len);
            let _ = CloseHandle(h);
            r.ok()?;
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            path.rsplit('\\').next().map(str::to_lowercase)
        }
    }

    /// Close the pop-ups of earlier children (only windows of our class owned by a
    /// process running this same exe name).
    fn close_older() {
        let me = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
        let mut prev: Option<HWND> = None;
        loop {
            let Ok(h) = (unsafe { FindWindowExW(None, prev, CLASS, PCWSTR::null()) }) else { break };
            if h.is_invalid() {
                break;
            }
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
            if pid != std::process::id() && me.is_some() && exe_name_of(pid) == me {
                unsafe {
                    let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
            prev = Some(h);
        }
    }

    pub fn show(note: &Note) -> Result<(), String> {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        // One pop-up at a time: take the mutex, asking an older pop-up to close first.
        let mutex = unsafe { CreateMutexW(None, false, w!("Local\\HoldMyRAM.NotifyPopup")) }.ok();
        if let Some(m) = mutex {
            if unsafe { WaitForSingleObject(m, 0) } == WAIT_TIMEOUT {
                close_older();
                // Abandoned (older child exited) counts as acquired; on timeout show anyway.
                let _ = unsafe { WaitForSingleObject(m, 1500) };
            }
        }

        let mon = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !unsafe { GetMonitorInfoW(mon, &mut mi) }.as_bool() {
            return Err("no primary monitor".into());
        }
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = unsafe { GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
        let scale = dx.max(96) as f32 / 96.0;

        let w = px(scale, 340);
        let text_w = w - px(scale, 36 + 16);
        let mut title: Vec<u16> = note.title.encode_utf16().collect();
        let mut detail: Vec<u16> = note.detail.encode_utf16().collect();
        let ft = font(scale, 14, 600);
        let fd = font(scale, 12, 400);
        let title_h = measure(&mut title, ft, text_w, 1);
        let detail_h = if detail.is_empty() { 0 } else { measure(&mut detail, fd, text_w, 3) };
        unsafe {
            let _ = DeleteObject(ft.into());
            let _ = DeleteObject(fd.into());
        }
        let h = px(scale, 12) + title_h + if detail_h > 0 { px(scale, 3) + detail_h } else { 0 } + px(scale, 13);
        let margin = px(scale, 12);
        let work = mi.rcWork;
        let x = work.right - w - margin;
        let y = work.bottom - h - margin;
        let dot = match note.kind {
            NoteKind::On => rgb(0x3f, 0xb9, 0x50),
            NoteKind::Off => rgb(0x3b, 0x8e, 0xea),
            NoteKind::Warn => rgb(0xe3, 0xa0, 0x08),
        };
        let _ = LAYOUT.set(Layout { title, detail, dot, scale, title_h, detail_h, w });

        unsafe {
            let hinst = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: hinst.into(),
                lpszClassName: CLASS,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            if RegisterClassExW(&wc) == 0 {
                return Err(format!("RegisterClassExW: {}", windows::core::Error::from_win32()));
            }
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                CLASS,
                w!("HoldMyRAM"),
                WS_POPUP,
                x,
                y,
                w,
                h,
                None,
                None,
                Some(hinst.into()),
                None,
            )
            .map_err(|e| format!("CreateWindowExW: {e}"))?;
            let r = px(scale, 16);
            let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, r, r);
            SetWindowRgn(hwnd, Some(rgn), false);
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA);
            let _ = START.set(Instant::now());
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetTimer(Some(hwnd), TIMER_FADE, 15, None);
            // Never outlive a few seconds, whatever happens to the fade timer.
            SetTimer(Some(hwnd), TIMER_SAFETY, 5000, None);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if let Some(m) = mutex {
            unsafe {
                let _ = CloseHandle(m);
            }
        }
        Ok(())
    }

    fn paint(hwnd: HWND) {
        let Some(l) = LAYOUT.get() else { return };
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let bg = CreateSolidBrush(rgb(0x20, 0x21, 0x24));
            FillRect(dc, &rc, bg);
            let _ = DeleteObject(bg.into());
            // Hairline border.
            let border = CreateSolidBrush(rgb(0x3c, 0x3d, 0x42));
            let r = px(l.scale, 16);
            let rgn = CreateRoundRectRgn(0, 0, rc.right + 1, rc.bottom + 1, r, r);
            let _ = FrameRgn(dc, rgn, border, 1, 1);
            let _ = DeleteObject(rgn.into());
            let _ = DeleteObject(border.into());
            // Dot, centred on the title line.
            let top = px(l.scale, 12);
            let d = px(l.scale, 10);
            let cx = px(l.scale, 16);
            let cy = top + l.title_h / 2 - d / 2;
            let dot = CreateSolidBrush(l.dot);
            let old_b = SelectObject(dc, dot.into());
            let old_p = SelectObject(dc, GetStockObject(NULL_PEN));
            let _ = Ellipse(dc, cx, cy, cx + d + 1, cy + d + 1);
            SelectObject(dc, old_p);
            SelectObject(dc, old_b);
            let _ = DeleteObject(dot.into());
            // Text.
            SetBkMode(dc, TRANSPARENT);
            let left = px(l.scale, 36);
            let right = l.w - px(l.scale, 16);
            let ft = font(l.scale, 14, 600);
            let old_f = SelectObject(dc, ft.into());
            SetTextColor(dc, rgb(0xf1, 0xf1, 0xf3));
            let mut t = l.title.clone();
            let mut r1 = RECT { left, top, right, bottom: top + l.title_h };
            DrawTextW(dc, &mut t, &mut r1, DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX);
            if l.detail_h > 0 {
                let fd = font(l.scale, 12, 400);
                SelectObject(dc, fd.into());
                SetTextColor(dc, rgb(0xb4, 0xb6, 0xbc));
                let mut dt = l.detail.clone();
                let y = top + l.title_h + px(l.scale, 3);
                let mut r2 = RECT { left, top: y, right, bottom: y + l.detail_h };
                DrawTextW(dc, &mut dt, &mut r2, DT_WORDBREAK | DT_END_ELLIPSIS | DT_EDITCONTROL | DT_NOPREFIX);
                SelectObject(dc, ft.into());
                let _ = DeleteObject(fd.into());
            }
            SelectObject(dc, old_f);
            let _ = DeleteObject(ft.into());
            let _ = EndPaint(hwnd, &ps);
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        match msg {
            // Never take focus, never take clicks.
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_TIMER => {
                let ms = START.get().map(|s| s.elapsed().as_millis() as u64).unwrap_or(u64::MAX);
                match (wp.0, popup_alpha(ms)) {
                    (TIMER_FADE, Some(a)) => {
                        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), a, LWA_ALPHA);
                    }
                    _ => {
                        let _ = DestroyWindow(hwnd);
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_curve() {
        assert!(popup_alpha(0).unwrap() > 0);
        assert_eq!(popup_alpha(119), Some(240));
        assert_eq!(popup_alpha(500), Some(240));
        assert_eq!(popup_alpha(1699), Some(240));
        assert!(popup_alpha(1850).unwrap() < 240);
        assert!(popup_alpha(1999).unwrap() < 10);
        assert_eq!(popup_alpha(2000), None);
        assert_eq!(popup_alpha(u64::MAX), None);
        // Monotonic in, flat, monotonic out.
        let mut last = 0;
        for m in 0..120 {
            let a = popup_alpha(m).unwrap();
            assert!(a >= last);
            last = a;
        }
        for m in 1700..2000 {
            let a = popup_alpha(m).unwrap();
            assert!(a <= last);
            last = a;
        }
    }

    #[test]
    fn toast_xml_escapes_everything() {
        let n = Note {
            kind: NoteKind::On,
            title: "a<b>&\"c'".into(),
            detail: "</text><audio src=\"ms-winsoundevent:Notification.Looping.Alarm\"/>".into(),
            sound: false,
            sound_set: holdmyram_core::config::NotifySoundSet::Speech,
        };
        let x = toast_xml(&n);
        assert!(x.contains("a&lt;b&gt;&amp;&quot;c&apos;"));
        assert_eq!(x.matches("<text").count(), 2);
        assert_eq!(x.matches("<audio").count(), 1);
        assert!(x.contains("<audio silent=\"true\"/>"));
        assert!(!x.contains("Looping.Alarm\"/>"));
    }

    #[test]
    fn missing_sound_file_falls_back_to_the_system_sound() {
        use holdmyram_core::config::NotifySoundSet;
        use std::path::{Path, PathBuf};
        let media = Path::new(r"C:\Windows\Media");
        // Present: the fixed file inside the media folder.
        assert_eq!(
            resolve(NotifySoundSet::Speech, NoteKind::On, Some(media), |_| true),
            Resolved::File(PathBuf::from(r"C:\Windows\Media\Speech On.wav"))
        );
        assert_eq!(
            resolve(NotifySoundSet::Chimes, NoteKind::Off, Some(media), |_| true),
            Resolved::File(PathBuf::from(r"C:\Windows\Media\chord.wav"))
        );
        for set in NotifySoundSet::ALL {
            for kind in [NoteKind::On, NoteKind::Off, NoteKind::Warn] {
                // Every file stays directly inside the media folder.
                if let Resolved::File(p) = resolve(set, kind, Some(media), |_| true) {
                    assert_eq!(p.parent(), Some(media), "{p:?}");
                }
                // Missing file, or no media folder: the system alias for that kind.
                let fallback = Resolved::Alias(notify::alias_for(kind));
                assert_eq!(resolve(set, kind, Some(media), |_| false), fallback);
                assert_eq!(resolve(set, kind, None, |_| true), fallback);
            }
            assert_eq!(resolve(set, NoteKind::Warn, Some(media), |_| true), Resolved::Alias("SystemExclamation"));
        }
        assert_eq!(resolve(NotifySoundSet::Device, NoteKind::On, Some(media), |_| true), Resolved::Alias("DeviceConnect"));
        assert_eq!(resolve(NotifySoundSet::Device, NoteKind::Off, Some(media), |_| true), Resolved::Alias("DeviceDisconnect"));
    }
}
