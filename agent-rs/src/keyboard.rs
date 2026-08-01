//! Keyboard layout and modifier mapping for the nested compositor.
//!
//! Nothing is remapped by hand: Veracage passes the host desktop's own XKB
//! configuration (model, layout, variant, options) to the compositor, which
//! hands it to libxkbcommon. Without this the sandbox gets libxkbcommon's
//! built-in default (a plain US layout), so a host that swaps Ctrl and Win, or
//! puts Compose on Caps Lock, behaves differently inside Veracage than outside.
//!
//! Settings can override the *modifier* part of it, using the standard XKB
//! option groups rather than a mapping of our own.

/// Modifier-mapping choices offered in Settings, in display order: the config
/// value with its label. "system" follows the host desktop, "none" strips any
/// modifier remapping the host has, and the rest are XKB options verbatim (see
/// `/usr/share/X11/xkb/rules/base.lst`).
pub const CHOICES: &[(&str, &str)] = &[
    ("system", "Host setting"),
    ("none", "No remapping"),
    ("altwin:ctrl_win", "Ctrl also acts as Win"),
    ("altwin:alt_win", "Alt also acts as Win"),
    ("altwin:ctrl_alt_win", "Ctrl acts as Alt, Alt as Win"),
    ("ctrl:swap_lwin_lctl", "Swap Left Win and Left Ctrl"),
    ("ctrl:swap_lalt_lctl", "Swap Left Alt and Left Ctrl"),
    ("ctrl:nocaps", "Caps Lock as Ctrl"),
];

/// The XKB option groups the Settings choice owns. Picking a modifier mapping
/// replaces whatever the host had in these groups, and leaves the rest of the
/// host's options (Compose key, currency signs, layout switching) alone.
const MODIFIER_GROUPS: &[&str] = &["altwin", "ctrl"];

pub fn is_valid(choice: &str) -> bool {
    CHOICES.iter().any(|(value, _)| *value == choice)
}

pub fn label(choice: &str) -> &'static str {
    CHOICES
        .iter()
        .find(|(value, _)| *value == choice)
        .map(|(_, label)| *label)
        .unwrap_or("Host setting")
}

/// A host desktop's XKB configuration. Empty fields mean "libxkbcommon's
/// default", which is what an undetectable desktop gets.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HostKeyboard {
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: String,
}

/// The host desktop's keyboard configuration, if detectable. KDE keeps it in
/// `kxkbrc` (`Use=false` means it leaves the layout to the system, so we do
/// too). Other desktops fall through to empty, and the compositor then uses
/// libxkbcommon's default.
pub fn host_keyboard() -> HostKeyboard {
    let read = |key: &str| -> Option<String> {
        for tool in ["kreadconfig6", "kreadconfig5"] {
            if let Some(v) = crate::fonts::run(tool, &["--file", "kxkbrc", "--group", "Layout", "--key", key]) {
                return Some(v);
            }
        }
        None
    };
    if read("Use").as_deref() != Some("true") {
        return HostKeyboard::default();
    }
    HostKeyboard {
        model: read("Model").unwrap_or_default(),
        layout: read("LayoutList").unwrap_or_default(),
        variant: read("VariantList").unwrap_or_default(),
        options: read("Options").unwrap_or_default(),
    }
}

/// The XKB option list to publish: the host's options with the configured
/// modifier mapping applied. "system" keeps the host's as they are, anything
/// else replaces the host's `altwin:`/`ctrl:` entries ("none" just removes
/// them).
pub fn options_for(host_options: &str, choice: &str) -> String {
    if choice == "system" {
        return host_options.to_string();
    }
    let group_of = |opt: &str| opt.split(':').next().unwrap_or("").to_string();
    let mut opts: Vec<&str> = host_options
        .split(',')
        .map(str::trim)
        .filter(|o| !o.is_empty() && !MODIFIER_GROUPS.contains(&group_of(o).as_str()))
        .collect();
    if choice != "none" {
        opts.push(choice);
    }
    opts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_keeps_the_host_options_verbatim() {
        let host = "compose:caps,eurosign:e,altwin:ctrl_win";
        assert_eq!(options_for(host, "system"), host);
    }

    #[test]
    fn a_choice_replaces_only_the_modifier_groups() {
        // The Compose key and the currency sign survive, the host's own
        // modifier mapping does not.
        assert_eq!(
            options_for("compose:caps,eurosign:e,altwin:ctrl_win", "ctrl:nocaps"),
            "compose:caps,eurosign:e,ctrl:nocaps"
        );
        assert_eq!(
            options_for("altwin:ctrl_win,ctrl:nocaps", "altwin:alt_win"),
            "altwin:alt_win"
        );
    }

    #[test]
    fn none_strips_the_modifier_mapping_and_nothing_else() {
        assert_eq!(
            options_for("compose:caps,altwin:ctrl_win", "none"),
            "compose:caps"
        );
        assert_eq!(options_for("", "none"), "");
    }

    #[test]
    fn every_offered_choice_is_accepted_and_labelled() {
        for (value, label) in CHOICES {
            assert!(is_valid(value));
            assert_eq!(super::label(value), *label);
        }
        assert!(!is_valid("ctrl:rm -rf"));
    }
}
