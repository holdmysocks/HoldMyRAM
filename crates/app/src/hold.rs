//! Hold-to-confirm for the global hotkey.
//!
//! No hook and nothing idle: RegisterHotKey (through the global-shortcut plugin) reports
//! the press; in hold mode one short-lived watcher thread then reads the main key's state
//! with GetAsyncKeyState every ~15 ms for at most `hold_ms` and ends. The plugin registers
//! with MOD_NOREPEAT, so auto-repeat sends no further presses; any press that still
//! arrives while a watcher runs is ignored.
//!
//! The pure parts (VK mapping, the timing decision) take injectable inputs and are
//! unit-tested; `key_down` is the only Win32 call.

use std::time::Duration;
use tauri_plugin_global_shortcut::{Code, Shortcut};

/// Poll step while the key is held.
pub const STEP: Duration = Duration::from_millis(15);

/// What a hold watch decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldOutcome {
    /// Still down when `hold` had passed: toggle.
    Confirmed,
    /// Released before `hold`: do nothing.
    Released,
}

/// Wait while `is_down()` holds, up to `hold`. `now` is time since an arbitrary origin,
/// `sleep` waits. Checks the key before every time check, so a release seen at the very
/// end still cancels. Ends after at most `hold` plus one step.
pub fn watch_hold(
    hold: Duration,
    step: Duration,
    mut is_down: impl FnMut() -> bool,
    mut now: impl FnMut() -> Duration,
    mut sleep: impl FnMut(Duration),
) -> HoldOutcome {
    let start = now();
    loop {
        if !is_down() {
            return HoldOutcome::Released;
        }
        let elapsed = now().saturating_sub(start);
        if elapsed >= hold {
            return HoldOutcome::Confirmed;
        }
        sleep(step.min(hold - elapsed).max(Duration::from_millis(1)));
    }
}

/// The virtual-key code GetAsyncKeyState must watch for the hotkey's main key: the same
/// key the global-hotkey crate registers. None when hold mode cannot follow that key
/// (the caller then falls back to tap and says why).
pub fn vk_for(code: Code) -> Option<u16> {
    let letters = [
        Code::KeyA, Code::KeyB, Code::KeyC, Code::KeyD, Code::KeyE, Code::KeyF, Code::KeyG, Code::KeyH, Code::KeyI,
        Code::KeyJ, Code::KeyK, Code::KeyL, Code::KeyM, Code::KeyN, Code::KeyO, Code::KeyP, Code::KeyQ, Code::KeyR,
        Code::KeyS, Code::KeyT, Code::KeyU, Code::KeyV, Code::KeyW, Code::KeyX, Code::KeyY, Code::KeyZ,
    ];
    if let Some(i) = letters.iter().position(|c| *c == code) {
        return Some(0x41 + i as u16);
    }
    let digits = [
        Code::Digit0, Code::Digit1, Code::Digit2, Code::Digit3, Code::Digit4, Code::Digit5, Code::Digit6, Code::Digit7,
        Code::Digit8, Code::Digit9,
    ];
    if let Some(i) = digits.iter().position(|c| *c == code) {
        return Some(0x30 + i as u16);
    }
    let numpad = [
        Code::Numpad0, Code::Numpad1, Code::Numpad2, Code::Numpad3, Code::Numpad4, Code::Numpad5, Code::Numpad6,
        Code::Numpad7, Code::Numpad8, Code::Numpad9,
    ];
    if let Some(i) = numpad.iter().position(|c| *c == code) {
        return Some(0x60 + i as u16);
    }
    let fkeys = [
        Code::F1, Code::F2, Code::F3, Code::F4, Code::F5, Code::F6, Code::F7, Code::F8, Code::F9, Code::F10, Code::F11,
        Code::F12, Code::F13, Code::F14, Code::F15, Code::F16, Code::F17, Code::F18, Code::F19, Code::F20, Code::F21,
        Code::F22, Code::F23, Code::F24,
    ];
    if let Some(i) = fkeys.iter().position(|c| *c == code) {
        return Some(0x70 + i as u16);
    }
    Some(match code {
        Code::Equal => 0xBB,        // VK_OEM_PLUS
        Code::Comma => 0xBC,        // VK_OEM_COMMA
        Code::Minus => 0xBD,        // VK_OEM_MINUS
        Code::Period => 0xBE,       // VK_OEM_PERIOD
        Code::Semicolon => 0xBA,    // VK_OEM_1
        Code::Slash => 0xBF,        // VK_OEM_2
        Code::Backquote => 0xC0,    // VK_OEM_3
        Code::BracketLeft => 0xDB,  // VK_OEM_4
        Code::Backslash => 0xDC,    // VK_OEM_5
        Code::BracketRight => 0xDD, // VK_OEM_6
        Code::Quote => 0xDE,        // VK_OEM_7
        Code::Backspace => 0x08,
        Code::Tab => 0x09,
        Code::Space => 0x20,
        Code::Enter | Code::NumpadEnter => 0x0D,
        Code::CapsLock => 0x14,
        Code::Escape => 0x1B,
        Code::PageUp => 0x21,
        Code::PageDown => 0x22,
        Code::End => 0x23,
        Code::Home => 0x24,
        Code::ArrowLeft => 0x25,
        Code::ArrowUp => 0x26,
        Code::ArrowRight => 0x27,
        Code::ArrowDown => 0x28,
        Code::PrintScreen => 0x2C,
        Code::Insert => 0x2D,
        Code::Delete => 0x2E,
        Code::NumLock => 0x90,
        Code::ScrollLock => 0x91,
        Code::Pause => 0x13,
        Code::NumpadAdd => 0x6B,
        Code::NumpadSubtract => 0x6D,
        Code::NumpadMultiply => 0x6A,
        Code::NumpadDecimal => 0x6E,
        Code::NumpadDivide => 0x6F,
        // Media and volume keys are often handled by the keyboard firmware or a vendor
        // tool and their key-up timing is unreliable; NumpadEqual is registered by the
        // global-hotkey crate as VK_E. Hold mode does not follow these.
        _ => return None,
    })
}

/// Hold settings for the registered shortcut: Ok(Some((vk, ms))) in hold mode,
/// Ok(None) in tap mode, Err(note) when hold mode was asked for but cannot be used with
/// this key (the hotkey then works on a tap).
pub fn plan(hold_mode: bool, hold_ms: u32, registered: Option<&Shortcut>) -> Result<Option<(u16, Duration)>, String> {
    if !hold_mode {
        return Ok(None);
    }
    let Some(s) = registered else {
        return Err("no hotkey is registered, so hold-to-confirm has nothing to follow".into());
    };
    match vk_for(s.key) {
        Some(vk) => Ok(Some((vk, Duration::from_millis(hold_ms as u64)))),
        None => Err(format!(
            "hold-to-confirm cannot follow the key {:?}; the hotkey works on a tap instead. Pick a letter, digit, F-key or another common key to use hold",
            s.key
        )),
    }
}

/// The physical key is down right now.
pub fn key_down(vk: u16) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    // High bit set: down.
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use tauri_plugin_global_shortcut::Modifiers;

    /// Runs `watch_hold` on a fake clock. `down_until`: the key reads as down while the
    /// fake time is below it (ms). Returns the outcome and the fake time at the end.
    fn run(hold_ms: u64, down_until: u64) -> (HoldOutcome, u64, u32) {
        let t = Cell::new(0u64);
        let polls = Cell::new(0u32);
        let out = watch_hold(
            Duration::from_millis(hold_ms),
            STEP,
            || {
                polls.set(polls.get() + 1);
                t.get() < down_until
            },
            || Duration::from_millis(t.get()),
            |d| t.set(t.get() + d.as_millis() as u64),
        );
        (out, t.get(), polls.get())
    }

    #[test]
    fn hold_long_enough_confirms_exactly_at_hold() {
        let (o, end, _) = run(700, u64::MAX);
        assert_eq!(o, HoldOutcome::Confirmed);
        assert_eq!(end, 700, "never sleeps past the deadline");
        let (o, end, _) = run(200, 10_000);
        assert_eq!((o, end), (HoldOutcome::Confirmed, 200));
        // Released after the deadline: still confirmed at the deadline.
        let (o, _, _) = run(700, 701);
        assert_eq!(o, HoldOutcome::Confirmed);
    }

    #[test]
    fn early_release_cancels() {
        for up in [0, 1, 14, 15, 300, 699] {
            let (o, end, _) = run(700, up);
            assert_eq!(o, HoldOutcome::Released, "released at {up}");
            assert!(end <= up + STEP.as_millis() as u64, "noticed within one step ({up} -> {end})");
        }
        // Released at exactly the deadline: the key check comes first and wins.
        let (o, _, _) = run(700, 700);
        assert_eq!(o, HoldOutcome::Released);
    }

    #[test]
    fn polling_is_bounded() {
        let (_, _, polls) = run(3000, u64::MAX);
        // 3000 / 15 = 200 steps, plus the first and last check.
        assert!(polls <= 202, "{polls}");
    }

    #[test]
    fn vk_mapping() {
        assert_eq!(vk_for(Code::KeyA), Some(0x41));
        assert_eq!(vk_for(Code::KeyR), Some(0x52));
        assert_eq!(vk_for(Code::KeyZ), Some(0x5A));
        assert_eq!(vk_for(Code::Digit0), Some(0x30));
        assert_eq!(vk_for(Code::Digit9), Some(0x39));
        assert_eq!(vk_for(Code::F1), Some(0x70));
        assert_eq!(vk_for(Code::F12), Some(0x7B));
        assert_eq!(vk_for(Code::F24), Some(0x87));
        assert_eq!(vk_for(Code::Numpad5), Some(0x65));
        assert_eq!(vk_for(Code::Space), Some(0x20));
        assert_eq!(vk_for(Code::ArrowLeft), Some(0x25));
        assert_eq!(vk_for(Code::ArrowDown), Some(0x28));
        assert_eq!(vk_for(Code::Home), Some(0x24));
        assert_eq!(vk_for(Code::Delete), Some(0x2E));
        assert_eq!(vk_for(Code::Backquote), Some(0xC0));
        assert_eq!(vk_for(Code::Pause), Some(0x13));
        for c in [Code::NumpadEqual, Code::AudioVolumeUp, Code::MediaPlayPause, Code::ShiftLeft, Code::Fn] {
            assert_eq!(vk_for(c), None, "{c:?}");
        }
    }

    #[test]
    fn plan_falls_back_with_a_note() {
        let s: Shortcut = "Ctrl+Alt+R".parse().unwrap();
        assert_eq!(plan(false, 700, Some(&s)), Ok(None));
        assert_eq!(plan(true, 700, Some(&s)), Ok(Some((0x52, Duration::from_millis(700)))));
        let s2 = Shortcut::new(Some(Modifiers::CONTROL), Code::AudioVolumeUp);
        let e = plan(true, 700, Some(&s2)).unwrap_err();
        assert!(e.contains("works on a tap"), "{e}");
        assert!(plan(true, 700, None).is_err());
        assert_eq!(plan(false, 700, None), Ok(None));
    }
}
