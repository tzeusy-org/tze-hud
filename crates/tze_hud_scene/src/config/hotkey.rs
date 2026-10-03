//! Global hotkey chord: parsing and Win32 encoding.
//!
//! Platform-independent so the config loader can validate a chord and the
//! (Windows-only) `RegisterHotKey` glue can consume the already-encoded values.

use std::fmt;

/// The non-modifier key of a chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyKey {
    /// `F1`..=`F24`.
    Function(u8),
    /// An uppercase ASCII letter or digit.
    Char(char),
}

/// A global hotkey chord: at least one modifier plus one key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
    pub key: HotkeyKey,
}

impl Hotkey {
    /// Default human safe-mode chord (Ctrl+Shift+F12). Ctrl+Shift+Esc is not
    /// usable: Windows reserves it for Task Manager.
    pub const DEFAULT_SAFE_MODE: Hotkey = Hotkey {
        ctrl: true,
        shift: true,
        alt: false,
        win: false,
        key: HotkeyKey::Function(12),
    };

    /// Parse a chord such as `"Ctrl+Shift+F12"` (case-insensitive, `+`-separated,
    /// modifiers `Ctrl|Shift|Alt|Win`, any order).
    pub fn parse(s: &str) -> Result<Hotkey, HotkeyParseError> {
        let (mut ctrl, mut shift, mut alt, mut win) = (false, false, false, false);
        let mut key = None;
        for part in s.split('+').map(str::trim) {
            let lower = part.to_ascii_lowercase();
            let dup = |flag: &mut bool| {
                let was = *flag;
                *flag = true;
                was
            };
            let duplicate = match lower.as_str() {
                "ctrl" | "control" => dup(&mut ctrl),
                "shift" => dup(&mut shift),
                "alt" => dup(&mut alt),
                "win" | "super" => dup(&mut win),
                _ => {
                    let parsed = parse_key(&lower).ok_or_else(|| {
                        HotkeyParseError(format!("unknown key {part:?} in chord {s:?}"))
                    })?;
                    key.replace(parsed).is_some()
                }
            };
            if duplicate {
                return Err(HotkeyParseError(format!(
                    "repeated element {part:?} in {s:?}"
                )));
            }
        }
        let key = key.ok_or_else(|| HotkeyParseError(format!("chord {s:?} has no key")))?;
        if !(ctrl || shift || alt || win) {
            return Err(HotkeyParseError(format!(
                "chord {s:?} needs at least one modifier (Ctrl, Shift, Alt, Win)"
            )));
        }
        Ok(Hotkey {
            ctrl,
            shift,
            alt,
            win,
            key,
        })
    }

    /// Win32 `MOD_*` mask for `RegisterHotKey` (`MOD_ALT=1, CONTROL=2, SHIFT=4, WIN=8`).
    pub fn win32_modifiers(&self) -> u32 {
        u32::from(self.alt)
            | u32::from(self.ctrl) << 1
            | u32::from(self.shift) << 2
            | u32::from(self.win) << 3
    }

    /// Win32 virtual-key code (`VK_F1 = 0x70`; letters and digits are their ASCII code).
    pub fn win32_vk(&self) -> u32 {
        match self.key {
            HotkeyKey::Function(n) => 0x70 + u32::from(n) - 1,
            HotkeyKey::Char(c) => u32::from(c),
        }
    }
}

fn parse_key(lower: &str) -> Option<HotkeyKey> {
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
        && (1..=24).contains(&n)
    {
        return Some(HotkeyKey::Function(n));
    }
    let mut chars = lower.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() => {
            Some(HotkeyKey::Char(c.to_ascii_uppercase()))
        }
        _ => None,
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (on, name) in [
            (self.ctrl, "Ctrl"),
            (self.shift, "Shift"),
            (self.alt, "Alt"),
            (self.win, "Win"),
        ] {
            if on {
                write!(f, "{name}+")?;
            }
        }
        match self.key {
            HotkeyKey::Function(n) => write!(f, "F{n}"),
            HotkeyKey::Char(c) => write!(f, "{c}"),
        }
    }
}

/// Why a chord string was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HotkeyParseError(pub String);

impl fmt::Display for HotkeyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HotkeyParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chords_and_encodes_for_win32() {
        let h = Hotkey::parse("ctrl + Shift+F12").unwrap();
        assert_eq!(h, Hotkey::DEFAULT_SAFE_MODE);
        assert_eq!((h.win32_modifiers(), h.win32_vk()), (0x6, 0x7B));
        assert_eq!(h.to_string(), "Ctrl+Shift+F12");
        let h = Hotkey::parse("Alt+Win+q").unwrap();
        assert_eq!((h.win32_modifiers(), h.win32_vk()), (0x9, u32::from('Q')));
    }

    #[test]
    fn rejects_malformed_chords() {
        for bad in [
            "F12",
            "Ctrl+Shift",
            "Ctrl+F25",
            "Ctrl+F0",
            "Ctrl+Ctrl+F1",
            "Ctrl+A+B",
            "Ctrl+Esc",
            "",
        ] {
            assert!(Hotkey::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }
}
