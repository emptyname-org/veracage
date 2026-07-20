//! Veracage keyboard shortcuts (the compositor-level clipboard transfers). The
//! human side publishes them to `/run/veracage/pub/shortcuts`; the compositor
//! reads them on its ~1s scan and matches them in `input.rs`. Bindings are a
//! `+`-joined combo like "Ctrl+Alt+C" with a single letter/digit key.

use smithay::input::keyboard::{Keysym, ModifiersState};

/// The two configurable actions and their default binds. Order matters (the
/// scan pairs by position when an action line is missing).
pub const DEFAULT_COPY_OUT: &str = "Ctrl+Alt+C";
pub const DEFAULT_PASTE_IN: &str = "Ctrl+Alt+V";

/// A parsed key combo: exact modifier set + a single lowercase ASCII key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keybind {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
    pub key: char,
}

impl Keybind {
    /// Human display, e.g. "Ctrl+Alt+C".
    pub fn label(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl+");
        }
        if self.alt {
            s.push_str("Alt+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        if self.logo {
            s.push_str("Super+");
        }
        s.push(self.key.to_ascii_uppercase());
        s
    }

    /// True if this bind is exactly satisfied by the current modifiers + key.
    pub fn matches(&self, mods: &ModifiersState, keysym: Keysym) -> bool {
        if self.ctrl != mods.ctrl
            || self.alt != mods.alt
            || self.shift != mods.shift
            || self.logo != mods.logo
        {
            return false;
        }
        // Compare case-insensitively: with Shift a letter keysym is uppercase.
        let raw = keysym.raw();
        char::from_u32(raw)
            .map(|c| c.to_ascii_lowercase() == self.key)
            .unwrap_or(false)
    }
}

/// Parse a "Ctrl+Alt+C" style bind. None on an unknown modifier or a non-single
/// key. The last `+`-part is the key; the rest are modifiers.
pub fn parse(s: &str) -> Option<Keybind> {
    let parts: Vec<&str> = s.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
    let (key_part, mods) = parts.split_last()?;
    let mut kb = Keybind { ctrl: false, alt: false, shift: false, logo: false, key: ' ' };
    for m in mods {
        match m.to_lowercase().as_str() {
            "ctrl" | "control" => kb.ctrl = true,
            "alt" => kb.alt = true,
            "shift" => kb.shift = true,
            "super" | "meta" | "logo" | "win" => kb.logo = true,
            _ => return None,
        }
    }
    let mut chars = key_part.chars();
    let c = chars.next()?;
    if chars.next().is_some() || !c.is_ascii_alphanumeric() {
        return None;
    }
    kb.key = c.to_ascii_lowercase();
    Some(kb)
}

/// The compositor's active binds. Defaults until `pub/shortcuts` is read.
#[derive(Clone, Debug)]
pub struct Binds {
    pub copy_out: Option<Keybind>,
    pub paste_in: Option<Keybind>,
}

impl Default for Binds {
    fn default() -> Self {
        Binds {
            copy_out: parse(DEFAULT_COPY_OUT),
            paste_in: parse(DEFAULT_PASTE_IN),
        }
    }
}

const PUB_DIR: &str = "/run/veracage/pub";

/// Read `pub/shortcuts` (`<action>\t<bind>` per line) into parsed binds. None if
/// the file is absent/unreadable (caller keeps its current binds).
pub fn scan() -> Option<Binds> {
    let path = std::path::Path::new(PUB_DIR).join("shortcuts");
    let md = std::fs::symlink_metadata(&path).ok()?;
    if !md.file_type().is_file() || md.len() > 4096 {
        return None;
    }
    let body = std::fs::read_to_string(&path).ok()?;
    let mut binds = Binds { copy_out: None, paste_in: None };
    for line in body.lines() {
        if let Some((action, bind)) = line.split_once('\t') {
            let kb = parse(bind.trim());
            match action.trim() {
                "copy_out" => binds.copy_out = kb,
                "paste_in" => binds.paste_in = kb,
                _ => {}
            }
        }
    }
    Some(binds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_combos() {
        let k = parse("Ctrl+Alt+C").unwrap();
        assert!(k.ctrl && k.alt && !k.shift && !k.logo && k.key == 'c');
        assert_eq!(parse("Shift+Alt+V").unwrap().key, 'v');
        assert_eq!(parse("Super+7").unwrap().key, '7');
        // Case-insensitive modifiers and key.
        assert_eq!(parse("ctrl+alt+x"), parse("Ctrl+Alt+X"));
    }

    #[test]
    fn parse_rejects_bad() {
        assert!(parse("").is_none());
        assert!(parse("Ctrl+Bogus+C").is_none()); // unknown modifier
        assert!(parse("Ctrl+Alt+CC").is_none()); // multi-char key
        assert!(parse("Ctrl+Alt++").is_none()); // non-alphanumeric key
    }

    #[test]
    fn label_round_trips_through_parse() {
        // label() emits a canonical modifier order (Ctrl, Alt, Shift, Super), so
        // test the stable invariant parse == parse(label(parse)) rather than
        // exact string equality with an arbitrarily-ordered input.
        for s in ["Ctrl+Alt+C", "Shift+Alt+V", "Super+9"] {
            let k = parse(s).unwrap();
            assert_eq!(parse(&k.label()).as_ref(), Some(&k));
        }
        // Canonical order is fixed regardless of input order.
        assert_eq!(parse("Shift+Alt+V").unwrap().label(), "Alt+Shift+V");
    }

    #[test]
    fn matches_requires_exact_modifiers() {
        let k = parse("Ctrl+Alt+C").unwrap();
        let c = Keysym::from_char('c');
        let mods = |ctrl, alt, shift, logo| ModifiersState {
            ctrl, alt, shift, logo, ..Default::default()
        };
        assert!(k.matches(&mods(true, true, false, false), c));
        assert!(!k.matches(&mods(true, false, false, false), c)); // missing Alt
        assert!(!k.matches(&mods(true, true, true, false), c)); // extra Shift
        // Case-insensitive against a Shift-uppercased keysym.
        assert!(k.matches(&mods(true, true, false, false), Keysym::from_char('C')));
    }
}
