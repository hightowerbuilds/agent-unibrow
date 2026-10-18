//! Keyboard and modifier translation into CEF key events.
//!
//! CEF's off-screen input expects Windows-style virtual key codes plus a
//! separate CHAR event for text, the way Chromium's own Windows port sees
//! keyboard input. Key names follow GPUI's (`enter`, `left`, `a`, `;`…).

use crate::Modifiers;
use cef::sys::cef_key_event_type_t as K;

pub const SHIFT: u32 = 1 << 1;
pub const CONTROL: u32 = 1 << 2;
pub const ALT: u32 = 1 << 3;
pub const LEFT_BUTTON: u32 = 1 << 4;
pub const MIDDLE_BUTTON: u32 = 1 << 5;
pub const RIGHT_BUTTON: u32 = 1 << 6;
pub const COMMAND: u32 = 1 << 7;

pub fn flags(m: Modifiers) -> u32 {
    (if m.shift { SHIFT } else { 0 })
        | (if m.control { CONTROL } else { 0 })
        | (if m.alt { ALT } else { 0 })
        | (if m.command { COMMAND } else { 0 })
}

/// A key press or release as the UI saw it.
pub struct KeyInput<'a> {
    /// GPUI key name: `a`, `enter`, `left`, `f5`, `;`.
    pub key: &'a str,
    /// The text this press would type, if any (already shifted).
    pub text: Option<&'a str>,
    pub modifiers: Modifiers,
    /// True for key down, false for key up.
    pub down: bool,
}

/// Windows virtual-key code for a GPUI key name.
pub fn virtual_key(key: &str) -> Option<i32> {
    let k = key.to_ascii_lowercase();
    if k.len() == 1 {
        let c = k.as_bytes()[0];
        return Some(match c {
            b'a'..=b'z' => (c - b'a' + 0x41) as i32,
            b'0'..=b'9' => (c - b'0' + 0x30) as i32,
            b';' => 0xBA,
            b'=' => 0xBB,
            b',' => 0xBC,
            b'-' => 0xBD,
            b'.' => 0xBE,
            b'/' => 0xBF,
            b'`' => 0xC0,
            b'[' => 0xDB,
            b'\\' => 0xDC,
            b']' => 0xDD,
            b'\'' => 0xDE,
            b' ' => 0x20,
            _ => return None,
        });
    }
    if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<i32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0x6F + n);
        }
    }
    Some(match k.as_str() {
        "backspace" => 0x08,
        "tab" => 0x09,
        "enter" => 0x0D,
        "shift" => 0x10,
        "control" | "ctrl" => 0x11,
        "alt" => 0x12,
        "escape" => 0x1B,
        "space" => 0x20,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "end" => 0x23,
        "home" => 0x24,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "insert" => 0x2D,
        "delete" => 0x2E,
        _ => return None,
    })
}

/// X11/XKB keycode (evdev code + 8) for a GPUI key name, US layout.
///
/// On Linux, Chromium derives the physical key (`KeyboardEvent.code`) from
/// this and ignores key events without one. GPUI doesn't expose the hardware
/// keycode, so it's recovered from the key name.
pub fn native_key(key: &str) -> i32 {
    let k = key.to_ascii_lowercase();
    if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<i32>().ok()) {
        return match n {
            1..=10 => 66 + n,
            11 => 95,
            12 => 96,
            _ => 0,
        };
    }
    match k.as_str() {
        "escape" => 9,
        "1" => 10, "2" => 11, "3" => 12, "4" => 13, "5" => 14,
        "6" => 15, "7" => 16, "8" => 17, "9" => 18, "0" => 19,
        "-" => 20, "=" => 21, "backspace" => 22, "tab" => 23,
        "q" => 24, "w" => 25, "e" => 26, "r" => 27, "t" => 28,
        "y" => 29, "u" => 30, "i" => 31, "o" => 32, "p" => 33,
        "[" => 34, "]" => 35, "enter" => 36, "control" | "ctrl" => 37,
        "a" => 38, "s" => 39, "d" => 40, "f" => 41, "g" => 42,
        "h" => 43, "j" => 44, "k" => 45, "l" => 46,
        ";" => 47, "'" => 48, "`" => 49, "shift" => 50, "\\" => 51,
        "z" => 52, "x" => 53, "c" => 54, "v" => 55, "b" => 56,
        "n" => 57, "m" => 58, "," => 59, "." => 60, "/" => 61,
        "alt" => 64, "space" | " " => 65,
        "home" => 110, "up" => 111, "pageup" => 112, "left" => 113,
        "right" => 114, "end" => 115, "down" => 116, "pagedown" => 117,
        "insert" => 118, "delete" => 119,
        _ => 0,
    }
}

fn event(kind: K, vk: i32, native: i32, ch: u16, modifiers: u32) -> cef::KeyEvent {
    cef::KeyEvent {
        size: std::mem::size_of::<cef::sys::cef_key_event_t>(),
        type_: cef::KeyEventType::from(kind),
        modifiers,
        windows_key_code: vk,
        native_key_code: native,
        is_system_key: (modifiers & ALT != 0) as i32,
        character: ch,
        unmodified_character: ch,
        focus_on_editable_field: 0,
    }
}

/// The CEF events for one key press or release.
pub fn translate(input: &KeyInput<'_>) -> Option<Vec<cef::KeyEvent>> {
    let flags = flags(input.modifiers);
    let vk = virtual_key(input.key);
    // A character typed with no shortcut modifiers (shift is fine).
    let typed: Option<u16> = input
        .text
        .filter(|_| !input.modifiers.control && !input.modifiers.alt && !input.modifiers.command)
        .and_then(|t| {
            let mut units = t.encode_utf16();
            let first = units.next()?;
            units.next().is_none().then_some(first)
        });

    let native = native_key(input.key);
    let char_for_key = match input.key {
        "enter" => Some(13u16),
        "tab" => Some(9u16),
        _ => typed,
    };
    if !input.down {
        return Some(vec![event(K::KEYEVENT_KEYUP, vk?, native, char_for_key.unwrap_or(0), flags)]);
    }
    // Same sequence as CEF's own Linux sample: RAWKEYDOWN, then CHAR for
    // keys that type something, both carrying the virtual key code.
    let mut out = Vec::with_capacity(2);
    match vk {
        Some(vk) => out.push(event(K::KEYEVENT_RAWKEYDOWN, vk, native, char_for_key.unwrap_or(0), flags)),
        None if typed.is_none() => return None,
        None => {}
    }
    if let Some(ch) = char_for_key {
        if !input.modifiers.control && !input.modifiers.alt && !input.modifiers.command {
            out.push(event(K::KEYEVENT_CHAR, vk.unwrap_or(ch as i32), native, ch, flags));
        }
    }
    Some(out)
}

/// CHAR events that type `text` (IME commits, pasted text).
pub fn text_events(text: &str) -> Vec<cef::KeyEvent> {
    text.encode_utf16().map(|u| event(K::KEYEVENT_CHAR, u as i32, 0, u, 0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down(key: &str, text: Option<&str>, m: Modifiers) -> Vec<(K, i32, u16)> {
        translate(&KeyInput { key, text, modifiers: m, down: true })
            .unwrap()
            .into_iter()
            .map(|e| (e.type_.into(), e.windows_key_code, e.character))
            .collect()
    }

    #[test]
    fn letter_sends_keydown_and_char() {
        let ev = down("a", Some("a"), Modifiers::default());
        assert_eq!(ev, vec![(K::KEYEVENT_RAWKEYDOWN, 0x41, 'a' as u16), (K::KEYEVENT_CHAR, 0x41, 'a' as u16)]);
    }

    #[test]
    fn ctrl_c_sends_no_char() {
        let ev = down("c", Some("c"), Modifiers { control: true, ..Default::default() });
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].1, 0x43);
    }

    #[test]
    fn enter_and_arrows() {
        assert_eq!(down("enter", None, Modifiers::default())[1], (K::KEYEVENT_CHAR, 0x0D, 13));
        assert_eq!(native_key("a"), 38);
        assert_eq!(native_key("enter"), 36);
        assert_eq!(down("left", None, Modifiers::default()), vec![(K::KEYEVENT_RAWKEYDOWN, 0x25, 0)]);
    }
}
