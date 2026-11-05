//! "Turned on / off" notifications.
//!
//! The tray process only builds a short message from the toggle's Report and starts a
//! detached child of its own exe (`holdmyram.exe --notify-toast <json>` or
//! `--notify-popup <json>`) that shows it and exits. The tray never loads WinRT, GDI
//! drawing or sound code for this; a short-lived waiter thread notes the child's exit
//! code (toast unavailable, ...) and ends. See `notify_child` for the child side.
//!
//! The child accepts exactly one argument after its flag: a JSON object with the fields
//! of [`Note`] and nothing else, at most [`MAX_ARG_BYTES`] bytes, plain text only.
//!
//! Sound preview (Settings "play" buttons): `holdmyram.exe --notify-sound <json>` with
//! exactly the fields of [`SoundReq`], at most [`MAX_SOUND_ARG_BYTES`] bytes; the child
//! plays one sound and exits. Sounds are named by set and kind only: the file names are
//! the fixed table in [`sound_for`], resolved inside `%SystemRoot%\Media` by the child.

use crate::debug_log;
use holdmyram_core::config::{NotifySoundSet, NotifyStyle};
use holdmyram_core::toggle::{Reason, Report};
use holdmyram_core::types::SelectionKind;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub const ARG_TOAST: &str = "--notify-toast";
pub const ARG_POPUP: &str = "--notify-popup";
pub const ARG_SOUND: &str = "--notify-sound";
pub const MAX_ARG_BYTES: usize = 1024;
pub const MAX_SOUND_ARG_BYTES: usize = 128;
pub const MAX_TITLE_CHARS: usize = 64;
pub const MAX_DETAIL_CHARS: usize = 200;

/// Child exit codes.
pub const EXIT_OK: i32 = 0;
pub const EXIT_BAD_ARGS: i32 = 2;
/// The toast failed; the pop-up was shown instead.
pub const EXIT_TOAST_FELL_BACK: i32 = 10;
/// Windows has notifications turned off for HoldMyRAM (or entirely); nothing shown.
pub const EXIT_TOAST_DISABLED: i32 = 11;
pub const EXIT_POPUP_FAILED: i32 = 12;
/// Sound preview: the set's file was missing or would not play; the system sound for
/// that kind was played instead.
pub const EXIT_SOUND_FELL_BACK: i32 = 13;
/// Sound preview: nothing could be played (sound scheme "No Sounds", no audio device...).
pub const EXIT_SOUND_FAILED: i32 = 14;

/// The dot colour and the sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteKind {
    /// Green dot.
    On,
    /// Blue dot.
    Off,
    /// Amber dot: refused, or not everything restored.
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Note {
    pub kind: NoteKind,
    pub title: String,
    pub detail: String,
    pub sound: bool,
    /// Which sounds, when `sound` is true. Required in the child argument (the tray and
    /// the child are the same exe).
    pub sound_set: NotifySoundSet,
}

impl Note {
    pub fn with_sound_set(mut self, set: NotifySoundSet) -> Note {
        self.sound_set = set;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildMode {
    Toast,
    Popup,
}

// ---------------------------------------------------------------------------------------
// Sounds (pure table; played only by the child)
// ---------------------------------------------------------------------------------------

/// A sound to play: a file name inside `%SystemRoot%\Media` (never a path), or a system
/// sound alias from the user's sound scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundRef {
    MediaFile(&'static str),
    Alias(&'static str),
}

/// The system sound for `kind`: the original sounds, and the fallback when a set's file
/// is missing. Warn is "Exclamation" for every set.
pub fn alias_for(kind: NoteKind) -> &'static str {
    match kind {
        NoteKind::On => "DeviceConnect",
        NoteKind::Off => "DeviceDisconnect",
        NoteKind::Warn => "SystemExclamation",
    }
}

/// The fixed table. Changing it is a code change, never a setting.
pub fn sound_for(set: NotifySoundSet, kind: NoteKind) -> SoundRef {
    use NoteKind::{Off, On, Warn};
    use NotifySoundSet::*;
    let file = match (set, kind) {
        (_, Warn) | (Device, _) => return SoundRef::Alias(alias_for(kind)),
        (Speech, On) => "Speech On.wav",
        (Speech, Off) => "Speech Off.wav",
        (Clicks, On) => "Windows Navigation Start.wav",
        (Clicks, Off) => "Windows Information Bar.wav",
        (Chimes, On) => "chimes.wav",
        (Chimes, Off) => "chord.wav",
        (Unlock, On) => "Windows Unlock.wav",
        (Unlock, Off) => "Windows Pop-up Blocked.wav",
        (Proximity, On) => "Windows Proximity Notification.wav",
        (Proximity, Off) => "Windows Balloon.wav",
    };
    SoundRef::MediaFile(file)
}

/// `on` or `off` (a preview never plays the warning sound).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewKind {
    On,
    Off,
}

impl PreviewKind {
    pub fn note_kind(self) -> NoteKind {
        match self {
            PreviewKind::On => NoteKind::On,
            PreviewKind::Off => NoteKind::Off,
        }
    }
}

/// The sound-preview child's argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoundReq {
    pub set: NotifySoundSet,
    pub kind: PreviewKind,
}

/// Validate `preview_sound`'s arguments: exact lowercase names only.
pub fn parse_preview(set: &str, kind: &str) -> Result<SoundReq, String> {
    let set = NotifySoundSet::from_name(set).ok_or_else(|| {
        let names: Vec<&str> = NotifySoundSet::ALL.iter().map(|s| s.as_str()).collect();
        format!("set must be one of {}, not {:?}", names.join(", "), clean(set, 32))
    })?;
    let kind = match kind {
        "on" => PreviewKind::On,
        "off" => PreviewKind::Off,
        _ => return Err(format!("kind must be \"on\" or \"off\", not {:?}", clean(kind, 32))),
    };
    Ok(SoundReq { set, kind })
}

// ---------------------------------------------------------------------------------------
// Message building (pure)
// ---------------------------------------------------------------------------------------

/// Plain single-line text: control characters become spaces, runs of spaces collapse,
/// at most `max` characters (an ellipsis marks a cut).
pub fn clean(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut last_space = true;
    for c in s.chars() {
        let c = if c.is_control() { ' ' } else { c };
        if c == ' ' {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(c);
            last_space = false;
        }
    }
    let out = out.trim_end().to_string();
    if out.chars().count() <= max {
        return out;
    }
    let mut cut: String = out.chars().take(max.saturating_sub(1)).collect();
    cut = cut.trim_end().to_string();
    cut.push('\u{2026}');
    cut
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Distinct (kind, name) pairs: one app with 30 processes is one item.
fn distinct<'a>(items: impl Iterator<Item = &'a holdmyram_core::toggle::Item>) -> usize {
    items.map(|i| (i.kind, i.name.to_lowercase())).collect::<HashSet<(SelectionKind, String)>>().len()
}

/// "1.4 GB" / "850 MB"; None below 1 MB.
pub fn fmt_ram(bytes: u64) -> Option<String> {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        Some(format!("{:.1} GB", b / GB))
    } else if b >= MB {
        Some(format!("{} MB", (b / MB).round() as u64))
    } else {
        None
    }
}

/// The notification for a finished toggle. `turning_on`: the toggle started from off.
pub fn message(r: &Report, turning_on: bool, sound: bool) -> Note {
    let note = |kind, title: &str, detail: String| Note {
        kind,
        title: clean(title, MAX_TITLE_CHARS),
        detail: clean(&detail, MAX_DETAIL_CHARS),
        sound,
        sound_set: NotifySoundSet::default(),
    };
    if turning_on {
        if let (Some(e), true) = (&r.error, r.acted.is_empty()) {
            return note(NoteKind::Warn, "HoldMyRAM could not turn on", format!("Nothing was changed: {e}"));
        }
        let mut parts = Vec::new();
        let n = distinct(r.acted.iter());
        parts.push(if n == 0 { "nothing needed closing".to_string() } else { format!("{} closed or stopped", plural(n, "item", "items")) });
        let freed: u64 = r.acted.iter().filter_map(|i| i.ram_bytes).sum();
        if let Some(f) = fmt_ram(freed) {
            parts.push(format!("about {f} freed"));
        }
        let has = |why: Reason| r.skipped.iter().any(|i| i.reason == Some(why));
        if has(Reason::UacCancelled) {
            parts.push("apps only, admin approval declined".into());
        } else if has(Reason::HelperFailed) {
            parts.push("apps only, the admin helper could not start".into());
        }
        let f = distinct(r.failed.iter());
        if f > 0 {
            parts.push(format!("{} could not be closed", plural(f, "item", "items")));
        }
        if has(Reason::Foreground) {
            parts.push("the app in front was left running".into());
        }
        let kind = if r.error.is_some() {
            parts.push("open HoldMyRAM for a warning".into());
            NoteKind::Warn
        } else {
            NoteKind::On
        };
        return note(kind, "HoldMyRAM is on", parts.join(", "));
    }
    if r.needs_admin > 0 {
        return note(
            NoteKind::Warn,
            "HoldMyRAM: apps restored",
            format!(
                "{} still waiting for admin approval; press the hotkey again to retry",
                plural(r.needs_admin, "service/task item", "service/task items")
            ),
        );
    }
    if let (Some(e), true) = (&r.error, r.acted.is_empty() && r.failed.is_empty()) {
        return note(NoteKind::Warn, "HoldMyRAM could not restore", e.clone());
    }
    let f = distinct(r.failed.iter());
    if f > 0 {
        return note(NoteKind::Warn, "HoldMyRAM is off", format!("{} could not be restored", plural(f, "item", "items")));
    }
    if r.error.is_some() {
        return note(NoteKind::Warn, "HoldMyRAM is off", "restored, with a warning: open HoldMyRAM for details".into());
    }
    note(NoteKind::Off, "HoldMyRAM is off", "everything restored".into())
}

/// The sample shown by `test_notification`.
pub fn sample(on: bool, sound: bool) -> Note {
    Note {
        kind: if on { NoteKind::On } else { NoteKind::Off },
        title: if on { "HoldMyRAM is on" } else { "HoldMyRAM is off" }.into(),
        detail: format!("Test: this is how turning {} will look", if on { "on" } else { "off" }),
        sound,
        sound_set: NotifySoundSet::default(),
    }
}

// ---------------------------------------------------------------------------------------
// Child arguments (strict)
// ---------------------------------------------------------------------------------------

pub fn encode(note: &Note) -> String {
    serde_json::to_string(note).expect("plain struct")
}

/// `args` as from `std::env::args()`. None: not a notification child. Some(Err): it is
/// one, but the arguments are not exactly `<flag> <json>` within the documented schema.
pub fn parse_child_args(args: &[String]) -> Option<Result<(ChildMode, Note), String>> {
    let mode = match args.get(1).map(String::as_str) {
        Some(ARG_TOAST) => ChildMode::Toast,
        Some(ARG_POPUP) => ChildMode::Popup,
        _ => return None,
    };
    Some((|| {
        if args.len() != 3 {
            return Err(format!("expected exactly one argument after {}, got {}", args[1], args.len().saturating_sub(2)));
        }
        let json = &args[2];
        if json.len() > MAX_ARG_BYTES {
            return Err(format!("argument is {} bytes, limit {MAX_ARG_BYTES}", json.len()));
        }
        let note: Note = serde_json::from_str(json).map_err(|e| format!("bad notification JSON: {e}"))?;
        let check = |what: &str, s: &str, max: usize| {
            if s.trim().is_empty() && what == "title" {
                return Err(format!("{what} is empty"));
            }
            if s.chars().count() > max {
                return Err(format!("{what} longer than {max} characters"));
            }
            if s.chars().any(char::is_control) {
                return Err(format!("{what} contains control characters"));
            }
            Ok(())
        };
        check("title", &note.title, MAX_TITLE_CHARS)?;
        check("detail", &note.detail, MAX_DETAIL_CHARS)?;
        Ok((mode, note))
    })())
}

/// Like [`parse_child_args`] for `--notify-sound <json>`. None: not a sound child.
pub fn parse_sound_args(args: &[String]) -> Option<Result<SoundReq, String>> {
    if args.get(1).map(String::as_str) != Some(ARG_SOUND) {
        return None;
    }
    Some((|| {
        if args.len() != 3 {
            return Err(format!("expected exactly one argument after {ARG_SOUND}, got {}", args.len().saturating_sub(2)));
        }
        let json = &args[2];
        if json.len() > MAX_SOUND_ARG_BYTES {
            return Err(format!("argument is {} bytes, limit {MAX_SOUND_ARG_BYTES}", json.len()));
        }
        serde_json::from_str(json).map_err(|e| format!("bad sound JSON: {e}"))
    })())
}

// ---------------------------------------------------------------------------------------
// Tray side
// ---------------------------------------------------------------------------------------

/// Last toast problem reported by a child (None after a toast worked).
static TOAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn set_toast_error(e: Option<String>) {
    *TOAST_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = e;
}

pub fn toast_error() -> Option<String> {
    TOAST_ERROR.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Windows build number (registry read once per process).
fn windows_build() -> u32 {
    use std::sync::OnceLock;
    static B: OnceLock<u32> = OnceLock::new();
    *B.get_or_init(|| {
        use windows::core::w;
        use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
        let mut buf = [0u16; 32];
        let mut len = (buf.len() * 2) as u32;
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
                w!("CurrentBuildNumber"),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut len),
            )
        };
        if r.is_err() {
            return 0;
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n]).trim().parse().unwrap_or(0)
    })
}

/// Toasts can be shown here: Windows 10 or later and no child has reported that toasts
/// fail or are turned off.
pub fn toast_available() -> bool {
    windows_build() >= 10240 && toast_error().is_none()
}

fn mode_for(style: NotifyStyle) -> Option<ChildMode> {
    match style {
        NotifyStyle::Toast => Some(ChildMode::Toast),
        NotifyStyle::Popup => Some(ChildMode::Popup),
        NotifyStyle::Off => None,
    }
}

fn spawn_child(mode: ChildMode, note: &Note) -> std::io::Result<std::process::Child> {
    let flag = match mode {
        ChildMode::Toast => ARG_TOAST,
        ChildMode::Popup => ARG_POPUP,
    };
    spawn_self(flag, &encode(note))
}

fn spawn_self(flag: &str, json: &str) -> std::io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new(std::env::current_exe()?)
        .arg(flag)
        .arg(json)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
}

/// Wait for the child's exit (no polling: one wait on its process handle) and return the
/// exit code, or None on timeout.
fn wait_child(child: &mut std::process::Child, timeout: Duration) -> Option<i32> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::WaitForSingleObject;
    let h = HANDLE(child.as_raw_handle());
    let r = unsafe { WaitForSingleObject(h, timeout.as_millis().min(u32::MAX as u128) as u32) };
    if r != WAIT_OBJECT_0 {
        return None;
    }
    child.try_wait().ok().flatten().and_then(|s| s.code())
}

/// Record what a child's exit code says about toasts; returns an error text for codes
/// other than success.
fn note_exit(mode: ChildMode, code: Option<i32>) -> Result<(), String> {
    let r = match code {
        Some(EXIT_OK) => {
            if mode == ChildMode::Toast {
                set_toast_error(None);
            }
            Ok(())
        }
        Some(EXIT_TOAST_FELL_BACK) => {
            let m = "the Windows notification could not be shown, so HoldMyRAM's own pop-up was shown instead (details in the log)".to_string();
            set_toast_error(Some(m.clone()));
            Err(m)
        }
        Some(EXIT_TOAST_DISABLED) => {
            let m = "Windows notifications are turned off for HoldMyRAM or for this PC (Settings > System > Notifications)".to_string();
            set_toast_error(Some(m.clone()));
            Err(m)
        }
        Some(EXIT_POPUP_FAILED) => Err("the pop-up could not be shown (details in the log)".into()),
        Some(EXIT_BAD_ARGS) => Err("internal error: the notification was refused as malformed".into()),
        Some(c) => Err(format!("the notification helper ended with code {c}")),
        None => Err("the notification helper did not finish in time".into()),
    };
    if let Err(e) = &r {
        debug_log(&format!("notify: {e}"));
    }
    r
}

/// After a toggle: show `note` in `style`, never blocking. The child runs detached; one
/// short-lived thread waits for it to exit to record toast problems, then ends.
pub fn show(style: NotifyStyle, note: Note) {
    let Some(mode) = mode_for(style) else { return };
    let mut child = match spawn_child(mode, &note) {
        Ok(c) => c,
        Err(e) => {
            debug_log(&format!("notify: could not start the notification helper: {e}"));
            return;
        }
    };
    let spawned = std::thread::Builder::new().name("notify-wait".into()).spawn(move || {
        let code = wait_child(&mut child, Duration::from_secs(15));
        let _ = note_exit(mode, code);
    });
    if let Err(e) = spawned {
        debug_log(&format!("notify: no waiter thread ({e}); the notification still shows"));
    }
}

/// `test_notification({kind: "on"|"off"})`: a sample with the saved settings. Waits for
/// the helper (a pop-up takes about 2 s) and reports whether it worked.
#[tauri::command]
pub async fn test_notification(kind: String) -> Result<(), String> {
    let on = match kind.as_str() {
        "on" => true,
        "off" => false,
        _ => return Err(format!("kind must be \"on\" or \"off\", not {kind:?}")),
    };
    let (style, sound, set) = {
        let r = crate::rt();
        (r.config.notify_style, r.config.notify_sound, r.config.notify_sound_set)
    };
    let Some(mode) = mode_for(style) else {
        return Err("notifications are turned off; choose Windows notification or Small pop-up and save first".into());
    };
    crate::blocking(move || {
        let note = sample(on, sound).with_sound_set(set);
        let mut child = spawn_child(mode, &note).map_err(|e| format!("could not start the notification helper: {e}"))?;
        let code = wait_child(&mut child, Duration::from_secs(10));
        if code.is_none() {
            let _ = child.kill();
        }
        note_exit(mode, code)
    })
    .await
}

/// A preview child is playing (one at a time; a click meanwhile is ignored).
static PREVIEW_BUSY: AtomicBool = AtomicBool::new(false);

struct BusyGuard;
impl Drop for BusyGuard {
    fn drop(&mut self) {
        PREVIEW_BUSY.store(false, Ordering::SeqCst);
    }
}

fn preview_exit(code: Option<i32>) -> Result<(), String> {
    let r = match code {
        Some(EXIT_OK) => Ok(()),
        Some(EXIT_SOUND_FELL_BACK) => {
            debug_log("notify: preview: the sound file was missing or would not play; the system sound was played instead");
            Ok(())
        }
        Some(EXIT_SOUND_FAILED) => {
            Err("Windows could not play the sound (the sound scheme may be set to No Sounds, or there is no audio device)".into())
        }
        Some(EXIT_BAD_ARGS) => Err("internal error: the sound request was refused as malformed".into()),
        Some(c) => Err(format!("the sound helper ended with code {c}")),
        None => Err("the sound helper did not finish in time".into()),
    };
    if let Err(e) = &r {
        debug_log(&format!("notify: preview: {e}"));
    }
    r
}

/// `preview_sound({set, kind: "on"|"off"})`: play that sound now, whatever the saved
/// notification settings (an explicit audition from Settings). The sound is played by a
/// short-lived child (`--notify-sound`); the tray loads no audio code. While one preview
/// is playing, another request returns Ok without playing (no stacking).
#[tauri::command]
pub async fn preview_sound(set: String, kind: String) -> Result<(), String> {
    let req = parse_preview(&set, &kind)?;
    if PREVIEW_BUSY.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return Ok(());
    }
    let guard = BusyGuard;
    crate::blocking(move || {
        let _guard = guard;
        let json = serde_json::to_string(&req).expect("plain struct");
        let mut child = spawn_self(ARG_SOUND, &json).map_err(|e| format!("could not start the sound helper: {e}"))?;
        let code = wait_child(&mut child, Duration::from_secs(10));
        if code.is_none() {
            let _ = child.kill();
        }
        preview_exit(code)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use holdmyram_core::toggle::Item;
    use holdmyram_core::types::Method;

    fn proc_item(pid: u32, name: &str, ram: Option<u64>) -> Item {
        let mut i = Item {
            pid,
            name: name.into(),
            kind: SelectionKind::Process,
            method: Some(Method::Close),
            reason: None,
            detail: "closed gracefully".into(),
            ram_bytes: None,
        };
        i.ram_bytes = ram;
        i
    }

    fn admin(kind: SelectionKind, name: &str, reason: Option<Reason>) -> Item {
        Item { pid: 0, name: name.into(), kind, method: None, reason, detail: String::new(), ram_bytes: None }
    }

    const MB: u64 = 1024 * 1024;

    fn report(action: &'static str) -> Report {
        Report { action, ..Default::default() }
    }

    #[test]
    fn on_counts_apps_not_processes_and_sums_ram() {
        let mut r = report("apply");
        r.acted = vec![
            proc_item(1, "chrome.exe", Some(800 * MB)),
            proc_item(2, "chrome.exe", Some(700 * MB)),
            proc_item(3, "Discord.exe", Some(300 * MB)),
            admin(SelectionKind::Service, "SysMain", None),
        ];
        let n = message(&r, true, false);
        assert_eq!(n.kind, NoteKind::On);
        assert_eq!(n.title, "HoldMyRAM is on");
        assert_eq!(n.detail, "3 items closed or stopped, about 1.8 GB freed");
        assert!(!n.sound);
        // One item, small.
        let mut r = report("apply");
        r.acted = vec![proc_item(1, "a.exe", Some(5 * MB))];
        assert_eq!(message(&r, true, true).detail, "1 item closed or stopped, about 5 MB freed");
        assert!(message(&r, true, true).sound);
    }

    #[test]
    fn on_without_trustworthy_ram_omits_the_figure() {
        let mut r = report("apply");
        r.acted = vec![proc_item(1, "a.exe", None), admin(SelectionKind::Task, r"\X\Y", None)];
        assert_eq!(message(&r, true, false).detail, "2 items closed or stopped");
        let r = report("apply");
        assert_eq!(message(&r, true, false).detail, "nothing needed closing");
    }

    #[test]
    fn on_with_admin_declined_and_foreground_and_failures() {
        let mut r = report("apply");
        r.acted = vec![proc_item(1, "a.exe", Some(200 * MB))];
        r.skipped = vec![
            admin(SelectionKind::Service, "SysMain", Some(Reason::UacCancelled)),
            admin(SelectionKind::Task, r"\X", Some(Reason::UacCancelled)),
            Item { reason: Some(Reason::Foreground), ..proc_item(9, "game.exe", None) },
        ];
        r.failed = vec![proc_item(5, "b.exe", None)];
        let n = message(&r, true, false);
        assert_eq!(n.kind, NoteKind::On);
        assert_eq!(
            n.detail,
            "1 item closed or stopped, about 200 MB freed, apps only, admin approval declined, 1 item could not be closed, the app in front was left running"
        );
        assert!(n.detail.chars().count() <= MAX_DETAIL_CHARS);
    }

    #[test]
    fn refused_apply() {
        let mut r = report("apply");
        r.error = Some("journal unreadable; refusing to act until it is restored or removed".into());
        let n = message(&r, true, false);
        assert_eq!(n.kind, NoteKind::Warn);
        assert_eq!(n.title, "HoldMyRAM could not turn on");
        assert!(n.detail.starts_with("Nothing was changed: journal unreadable"), "{}", n.detail);
        // Partial: acted, then the journal update failed.
        let mut r = report("apply");
        r.acted = vec![proc_item(1, "a.exe", None)];
        r.error = Some("actions done but journal update failed".into());
        let n = message(&r, true, false);
        assert_eq!((n.kind, n.title.as_str()), (NoteKind::Warn, "HoldMyRAM is on"));
        assert!(n.detail.ends_with("open HoldMyRAM for a warning"));
    }

    #[test]
    fn off_clean() {
        let mut r = report("revert");
        r.acted = vec![proc_item(1, "a.exe", None)];
        let n = message(&r, false, true);
        assert_eq!((n.kind, n.title.as_str(), n.detail.as_str()), (NoteKind::Off, "HoldMyRAM is off", "everything restored"));
        let n = message(&report("revert"), false, false);
        assert_eq!(n.detail, "everything restored");
    }

    #[test]
    fn off_with_failures() {
        let mut r = report("revert");
        r.acted = vec![proc_item(1, "a.exe", None)];
        r.failed = vec![proc_item(2, "b.exe", None), proc_item(3, "b.exe", None), proc_item(4, "c.exe", None)];
        let n = message(&r, false, false);
        assert_eq!((n.kind, n.detail.as_str()), (NoteKind::Warn, "2 items could not be restored"));
    }

    #[test]
    fn off_with_admin_pending() {
        let mut r = report("revert");
        r.acted = vec![proc_item(1, "a.exe", None)];
        r.needs_admin = 1;
        let n = message(&r, false, false);
        assert_eq!(n.kind, NoteKind::Warn);
        assert_eq!(n.title, "HoldMyRAM: apps restored");
        assert_eq!(n.detail, "1 service/task item still waiting for admin approval; press the hotkey again to retry");
        r.needs_admin = 3;
        assert!(message(&r, false, false).detail.starts_with("3 service/task items still waiting"));
    }

    #[test]
    fn off_refused() {
        let mut r = report("revert");
        r.error = Some("journal unreadable".into());
        let n = message(&r, false, false);
        assert_eq!((n.kind, n.title.as_str(), n.detail.as_str()), (NoteKind::Warn, "HoldMyRAM could not restore", "journal unreadable"));
    }

    #[test]
    fn messages_are_clean_and_bounded() {
        let mut r = report("apply");
        r.error = Some(format!("bad\r\nthing\t{}", "x".repeat(1000)));
        let n = message(&r, true, false);
        assert!(n.detail.chars().count() <= MAX_DETAIL_CHARS);
        assert!(!n.detail.chars().any(char::is_control));
        assert!(n.detail.ends_with('\u{2026}'));
        // Every message passes the child's own checks.
        let args = vec!["x".into(), ARG_POPUP.into(), encode(&n)];
        assert!(matches!(parse_child_args(&args), Some(Ok(_))));
        assert_eq!(clean("  a \n\n b  ", 10), "a b");
        assert_eq!(fmt_ram(1024 * 1024 - 1), None);
        assert_eq!(fmt_ram(1536 * MB).as_deref(), Some("1.5 GB"));
    }

    fn args(flag: &str, json: &str) -> Vec<String> {
        vec!["holdmyram.exe".into(), flag.into(), json.into()]
    }

    #[test]
    fn child_args_round_trip() {
        let n = sample(true, true);
        let a = args(ARG_TOAST, &encode(&n));
        assert_eq!(parse_child_args(&a), Some(Ok((ChildMode::Toast, n.clone()))));
        let a = args(ARG_POPUP, &encode(&n));
        assert_eq!(parse_child_args(&a), Some(Ok((ChildMode::Popup, n))));
        // Other invocations are not notification children.
        assert_eq!(parse_child_args(&["holdmyram.exe".into()]), None);
        assert_eq!(parse_child_args(&["holdmyram.exe".into(), "--update-worker".into()]), None);
        assert_eq!(parse_child_args(&["holdmyram.exe".into(), "x".into(), ARG_TOAST.into()]), None);
    }

    #[test]
    fn child_args_are_strict() {
        let bad = |a: Vec<String>| match parse_child_args(&a) {
            Some(Err(e)) => e,
            other => panic!("accepted {a:?}: {other:?}"),
        };
        let good = encode(&sample(false, false));
        // Missing or extra arguments.
        bad(vec!["h".into(), ARG_TOAST.into()]);
        bad(vec!["h".into(), ARG_POPUP.into(), good.clone(), "extra".into()]);
        // Oversize.
        let big = format!(r#"{{"kind":"on","title":"t","detail":"{}","sound":false,"sound_set":"speech"}}"#, "a".repeat(MAX_ARG_BYTES));
        assert!(bad(args(ARG_POPUP, &big)).contains("limit"));
        // Malformed JSON, wrong types, unknown or missing fields, unknown kind or set.
        for j in [
            "",
            "not json",
            "[]",
            "{}",
            r#"{"kind":"on","title":"t","detail":"d","sound_set":"speech"}"#,
            r#"{"kind":"on","title":"t","detail":"d","sound":false}"#,
            r#"{"kind":"on","title":"t","detail":"d","sound":"yes","sound_set":"speech"}"#,
            r#"{"kind":"on","title":"t","detail":"d","sound":false,"sound_set":"speech","icon":"C:\\x.png"}"#,
            r#"{"kind":"on","title":"t","detail":"d","sound":false,"sound_set":"speech","xml":"<toast/>"}"#,
            r#"{"kind":"launch","title":"t","detail":"d","sound":false,"sound_set":"speech"}"#,
            r#"{"kind":"on","title":5,"detail":"d","sound":false,"sound_set":"speech"}"#,
            r#"{"kind":"on","title":"t","detail":"d","sound":false,"sound_set":"speech"} trailing"#,
            // Sound set: unknown, wrong case, wrong type, path or file name.
            r#"{"kind":"on","title":"t","detail":"d","sound":true,"sound_set":"beep"}"#,
            r#"{"kind":"on","title":"t","detail":"d","sound":true,"sound_set":"Speech"}"#,
            r#"{"kind":"on","title":"t","detail":"d","sound":true,"sound_set":1}"#,
            r#"{"kind":"on","title":"t","detail":"d","sound":true,"sound_set":null}"#,
            r#"{"kind":"on","title":"t","detail":"d","sound":true,"sound_set":["speech"]}"#,
            r#"{"kind":"on","title":"t","detail":"d","sound":true,"sound_set":"C:\\Windows\\Media\\chimes.wav"}"#,
            r#"{"kind":"on","title":"t","detail":"d","sound":true,"sound_set":"..\\..\\x.wav"}"#,
            r#"{"kind":"on","title":"t","detail":"d","sound":true,"sound_set":"chimes.wav"}"#,
            r#"{"kind":"on","title":"t","detail":"d","sound":true,"sound_set":"speech","sound_file":"x.wav"}"#,
        ] {
            bad(args(ARG_TOAST, j));
        }
        // Field contents.
        let with = |t: &str, d: &str| {
            encode(&Note { kind: NoteKind::On, title: t.into(), detail: d.into(), sound: false, sound_set: NotifySoundSet::Speech })
        };
        assert!(bad(args(ARG_POPUP, &with("", "d"))).contains("empty"));
        assert!(bad(args(ARG_POPUP, &with(&"t".repeat(MAX_TITLE_CHARS + 1), "d"))).contains("longer"));
        assert!(bad(args(ARG_POPUP, &with("t", &"d".repeat(MAX_DETAIL_CHARS + 1)))).contains("longer"));
        assert!(bad(args(ARG_POPUP, &with("t\n", "d"))).contains("control"));
        assert!(bad(args(ARG_POPUP, &with("t", "d\u{1b}[2J"))).contains("control"));
        // Markup is plain text to the child (escaped before it reaches the toast XML).
        assert!(matches!(parse_child_args(&args(ARG_TOAST, &with("<b>&", "</text><audio src='x'/>"))), Some(Ok(_))));
    }

    #[test]
    fn sound_set_travels_in_the_child_argument() {
        for set in NotifySoundSet::ALL {
            let n = sample(false, true).with_sound_set(set);
            let a = args(ARG_POPUP, &encode(&n));
            assert_eq!(parse_child_args(&a), Some(Ok((ChildMode::Popup, n))));
        }
        // message() defaults to the default set; the caller sets the saved one.
        let n = message(&report("revert"), false, true);
        assert_eq!(n.sound_set, NotifySoundSet::Speech);
        assert_eq!(n.with_sound_set(NotifySoundSet::Clicks).sound_set, NotifySoundSet::Clicks);
    }

    #[test]
    fn sound_table_is_fixed_file_names() {
        use NoteKind::{Off, On, Warn};
        use NotifySoundSet::*;
        let expect = [
            (Speech, "Speech On.wav", "Speech Off.wav"),
            (Clicks, "Windows Navigation Start.wav", "Windows Information Bar.wav"),
            (Chimes, "chimes.wav", "chord.wav"),
            (Unlock, "Windows Unlock.wav", "Windows Pop-up Blocked.wav"),
            (Proximity, "Windows Proximity Notification.wav", "Windows Balloon.wav"),
        ];
        for (set, on, off) in expect {
            assert_eq!(sound_for(set, On), SoundRef::MediaFile(on));
            assert_eq!(sound_for(set, Off), SoundRef::MediaFile(off));
        }
        assert_eq!(sound_for(Device, On), SoundRef::Alias("DeviceConnect"));
        assert_eq!(sound_for(Device, Off), SoundRef::Alias("DeviceDisconnect"));
        for set in NotifySoundSet::ALL {
            // Warn is Exclamation everywhere.
            assert_eq!(sound_for(set, Warn), SoundRef::Alias("SystemExclamation"));
            for kind in [On, Off, Warn] {
                match sound_for(set, kind) {
                    SoundRef::MediaFile(f) => {
                        assert!(!f.contains(['\\', '/', ':', '\0']) && !f.contains(".."), "{f}");
                        assert!(f.ends_with(".wav") && f.trim() == f && !f.is_empty(), "{f}");
                    }
                    SoundRef::Alias(a) => assert!(a.chars().all(|c| c.is_ascii_alphanumeric()), "{a}"),
                }
            }
        }
    }

    #[test]
    fn preview_arguments_are_validated() {
        for set in NotifySoundSet::ALL {
            assert_eq!(parse_preview(set.as_str(), "on"), Ok(SoundReq { set, kind: PreviewKind::On }));
            assert_eq!(parse_preview(set.as_str(), "off"), Ok(SoundReq { set, kind: PreviewKind::Off }));
        }
        for (s, k) in [
            ("beep", "on"),
            ("Speech", "on"),
            ("", "on"),
            (" speech", "on"),
            (r"C:\Windows\Media\chimes.wav", "on"),
            ("chimes.wav", "on"),
            ("speech", "warn"),
            ("speech", "ON"),
            ("speech", ""),
            ("speech", "on "),
        ] {
            assert!(parse_preview(s, k).is_err(), "accepted {s:?} {k:?}");
        }
        // Errors never echo an unbounded value.
        let e = parse_preview(&"x".repeat(10_000), "on").unwrap_err();
        assert!(e.len() < 200, "{}", e.len());
    }

    #[test]
    fn sound_child_args_are_strict() {
        let sa = |j: &str| vec!["holdmyram.exe".to_string(), ARG_SOUND.to_string(), j.to_string()];
        let req = SoundReq { set: NotifySoundSet::Unlock, kind: PreviewKind::Off };
        let j = serde_json::to_string(&req).unwrap();
        assert_eq!(j, r#"{"set":"unlock","kind":"off"}"#);
        assert_eq!(parse_sound_args(&sa(&j)), Some(Ok(req)));
        // Every set and kind fits the size limit.
        for set in NotifySoundSet::ALL {
            for kind in [PreviewKind::On, PreviewKind::Off] {
                let j = serde_json::to_string(&SoundReq { set, kind }).unwrap();
                assert!(j.len() <= MAX_SOUND_ARG_BYTES);
                assert!(matches!(parse_sound_args(&sa(&j)), Some(Ok(_))));
            }
        }
        // Not a sound child.
        assert_eq!(parse_sound_args(&["h".into()]), None);
        assert_eq!(parse_sound_args(&args(ARG_POPUP, &j)), None);
        assert_eq!(parse_child_args(&sa(&j)), None);
        // Refused.
        let bad = |a: Vec<String>| assert!(matches!(parse_sound_args(&a), Some(Err(_))), "accepted {a:?}");
        bad(vec!["h".into(), ARG_SOUND.into()]);
        bad(vec!["h".into(), ARG_SOUND.into(), j.clone(), "x".into()]);
        let big = format!(r#"{{"set":"speech","kind":"on"{}}}"#, " ".repeat(MAX_SOUND_ARG_BYTES));
        match parse_sound_args(&sa(&big)) {
            Some(Err(e)) => assert!(e.contains("limit"), "{e}"),
            other => panic!("{other:?}"),
        }
        for j in [
            "",
            "{}",
            r#"{"set":"speech"}"#,
            r#"{"kind":"on"}"#,
            r#"{"set":"beep","kind":"on"}"#,
            r#"{"set":"speech","kind":"warn"}"#,
            r#"{"set":3,"kind":"on"}"#,
            r#"{"set":"speech","kind":true}"#,
            r#"{"set":"C:\\Windows\\Media\\chimes.wav","kind":"on"}"#,
            r#"{"set":"..\\x","kind":"on"}"#,
            r#"{"set":"speech","kind":"on","file":"x.wav"}"#,
            r#"{"set":"speech","kind":"on"} x"#,
        ] {
            bad(sa(j));
        }
    }

    #[test]
    fn preview_exit_codes() {
        assert_eq!(preview_exit(Some(EXIT_OK)), Ok(()));
        assert_eq!(preview_exit(Some(EXIT_SOUND_FELL_BACK)), Ok(()));
        assert!(preview_exit(Some(EXIT_SOUND_FAILED)).unwrap_err().contains("No Sounds"));
        assert!(preview_exit(Some(EXIT_BAD_ARGS)).is_err());
        assert!(preview_exit(None).unwrap_err().contains("in time"));
    }

    #[test]
    fn exit_codes_map_to_errors() {
        assert_eq!(note_exit(ChildMode::Popup, Some(EXIT_OK)), Ok(()));
        assert!(note_exit(ChildMode::Popup, Some(EXIT_POPUP_FAILED)).is_err());
        assert!(note_exit(ChildMode::Toast, None).unwrap_err().contains("in time"));
        assert!(note_exit(ChildMode::Toast, Some(EXIT_TOAST_DISABLED)).unwrap_err().contains("turned off"));
        assert!(toast_error().is_some());
        assert_eq!(note_exit(ChildMode::Toast, Some(EXIT_OK)), Ok(()));
        assert!(toast_error().is_none());
    }
}
