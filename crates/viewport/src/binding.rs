// SPDX-License-Identifier: GPL-3.0-or-later
//
// Keybindings. Ports src/binding.c.
//
// Almost every binding is a passthrough: layout is the shell's policy, so
// "split the container" or "move the window left" are messages, not
// compositor operations. What stays here is the handful that cannot be — a
// binding that must work when the shell is broken, and launching a process.

use smithay::input::keyboard::{keysyms, ModifiersState};

/// What a chord does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Run a command.
    Exec(String),
    /// Close the focused window.
    Close,
    /// Stop the compositor.
    Exit,
    /// Move focus to the neighbouring window, or step through them.
    ///
    /// A compositor action rather than a passthrough, as in `src/binding.c:88`
    /// — "the window to the left of this one" is about where things are on
    /// screen, which the compositor already knows, and it has to work across
    /// monitors where the shell's tree does not reach.
    Focus(String),
    /// Reload the shell, bypassing the HTTP cache.
    Reload,
    /// Switch the session between dark and light.
    ///
    /// Not the shell's: a client's colour scheme is answered over D-Bus by the
    /// settings portal, which the shell has no way to reach.
    Appearance,
    /// Hand the rest to the shell as a `shell.command`.
    ///
    /// The default for anything this does not implement itself, because the
    /// shell is what knows the layout — and passing a command it does not
    /// recognise costs one ignored message, where swallowing it here would
    /// look like a dead key.
    Shell(String),
}

/// One binding.
#[derive(Debug, Clone)]
pub struct Binding {
    pub modifiers: Modifiers,
    pub keysym: u32,
    pub action: Action,
}

/// The modifiers a chord requires.
///
/// Compared exactly: `Mod4+q` must not fire on `Mod4+Shift+q`, or a shifted
/// binding is unreachable because the unshifted one always matches first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub logo: bool,
}

impl Modifiers {
    pub fn from_state(state: &ModifiersState) -> Self {
        Self {
            shift: state.shift,
            ctrl: state.ctrl,
            alt: state.alt,
            logo: state.logo,
        }
    }
}

/// Parse one `Mod4+Shift+q=close` specification.
///
/// Everything before the final `+` is a modifier and the remainder is the key,
/// which keeps `Mod4+Shift+plus` unambiguous — splitting on every `+` would
/// leave an empty key name.
pub fn parse(spec: &str) -> Option<Binding> {
    let (chord, action) = spec.split_once('=')?;
    let action = action.trim();
    if chord.is_empty() || action.is_empty() {
        return None;
    }

    let binding = parse_chord(chord.trim())?;
    Some(Binding {
        action: parse_action(action),
        ..binding
    })
}

/// Parse a chord with no action, as `bind.add` sends it.
pub fn parse_chord(chord: &str) -> Option<Binding> {
    let mut modifiers = Modifiers::default();
    let mut rest = chord;

    // Left to right, stopping at the last '+': the key may itself be '+'.
    while let Some((name, remainder)) = rest.split_once('+') {
        match name.to_ascii_lowercase().as_str() {
            "shift" => modifiers.shift = true,
            "ctrl" | "control" => modifiers.ctrl = true,
            "alt" | "mod1" => modifiers.alt = true,
            "mod4" | "super" | "logo" => modifiers.logo = true,
            // An unknown modifier is not a key with a stray plus in front of
            // it; treating it as one would bind something arbitrary.
            _ => return None,
        }
        rest = remainder;
        if rest.is_empty() {
            return None;
        }
    }

    let keysym = keysym_from_name(rest)?;
    Some(Binding {
        modifiers,
        keysym,
        action: Action::Shell(String::new()),
    })
}

fn parse_action(action: &str) -> Action {
    match action.split_once(' ') {
        Some(("exec", rest)) => Action::Exec(rest.trim().to_owned()),
        Some(("shell", rest)) => Action::Shell(rest.trim().to_owned()),
        Some(("focus", rest)) if !rest.trim().is_empty() => {
            Action::Focus(rest.trim().to_owned())
        }
        _ => match action {
            "close" => Action::Close,
            "exit" => Action::Exit,
            "reload" => Action::Reload,
            "appearance toggle" => Action::Appearance,
            // Everything else is the shell's, including `focus left` and the
            // layout verbs.
            other => Action::Shell(other.to_owned()),
        },
    }
}

/// An xkb keysym by name.
fn keysym_from_name(name: &str) -> Option<u32> {
    use smithay::input::keyboard::xkb;

    // Case-sensitive first, so `a` and `A` stay distinct, then insensitive so
    // `return` works as well as `Return`.
    let strict = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
    if strict.raw() != keysyms::KEY_NoSymbol {
        return Some(strict.raw());
    }
    let loose = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE);
    (loose.raw() != keysyms::KEY_NoSymbol).then(|| loose.raw())
}

/// The bindings a session starts with. Ports `add_default` in src/binding.c.
///
/// Layout verbs are all passthroughs: the shell decides what splitting,
/// fullscreen and moving mean, and duplicating that judgement here would give
/// two things an opinion about it.
pub fn defaults(terminal: &str, menu: &str, scrolling: bool) -> Vec<Binding> {
    let mut specs: Vec<String> = vec![
        format!("Mod4+Return=exec {terminal}"),
        format!("Mod4+d=exec {menu}"),
        "Mod4+Shift+q=close".to_owned(),
        "Mod4+Shift+e=exit".to_owned(),
        "Mod4+Shift+c=reload".to_owned(),
        "Mod4+Shift+d=appearance toggle".to_owned(),
        "Mod4+Tab=focus next".to_owned(),
        "Mod4+Shift+Tab=focus prev".to_owned(),
        "Mod4+f=shell window.fullscreen".to_owned(),
        "Mod4+a=shell window.focus_parent".to_owned(),
        "Mod4+Shift+space=shell layout.float.toggle".to_owned(),
        "Mod4+b=shell layout.split horizontal".to_owned(),
        "Mod4+v=shell layout.split vertical".to_owned(),
    ];

    // sway's movement keys, and the arrows beside them.
    let directions = ["left", "down", "up", "right"];
    let letters = ["h", "j", "k", "l"];
    let arrows = ["Left", "Down", "Up", "Right"];
    for i in 0..4 {
        // Directional focus in a scrolling layout is not a geometry question:
        // the column you want is usually scrolled off the screen, so only the
        // shell can answer it. Sending both there keeps one implementation.
        // Tiling: the compositor answers it geometrically. Scrolling: the
        // column you want is usually scrolled off the screen, so only the
        // shell can (`src/binding.c:391`).
        let focus = if scrolling { "shell layout.focus" } else { "focus" };
        specs.push(format!("Mod4+{}={focus} {}", letters[i], directions[i]));
        specs.push(format!("Mod4+{}={focus} {}", arrows[i], directions[i]));
        specs.push(format!(
            "Mod4+Shift+{}=shell window.move {}",
            letters[i], directions[i]
        ));
        specs.push(format!(
            "Mod4+Shift+{}=shell window.move {}",
            arrows[i], directions[i]
        ));
    }

    // Workspaces.
    for workspace in 1..=9 {
        specs.push(format!("Mod4+{workspace}=shell workspace.switch {workspace}"));
        specs.push(format!(
            "Mod4+Shift+{workspace}=shell workspace.move {workspace}"
        ));
    }

    specs.iter().filter_map(|spec| parse(spec)).collect()
}

/// The action a chord fires, if any.
pub fn match_binding<'a>(
    bindings: &'a [Binding],
    modifiers: &ModifiersState,
    keysym: u32,
) -> Option<&'a Action> {
    let wanted = Modifiers::from_state(modifiers);
    bindings
        .iter()
        .find(|binding| binding.modifiers == wanted && binding.keysym == keysym)
        .map(|binding| &binding.action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chord_parses_into_modifiers_and_a_key() {
        let binding = parse("Mod4+Shift+q=close").expect("should parse");
        assert!(binding.modifiers.logo);
        assert!(binding.modifiers.shift);
        assert!(!binding.modifiers.ctrl);
        assert_eq!(binding.action, Action::Close);
        // Lowercase: the shift lives in the modifiers, not the key. Matching
        // has to use the unmodified keysym for the same reason.
        assert_eq!(binding.keysym, keysyms::KEY_q);
    }

    #[test]
    fn a_key_that_is_a_plus_still_parses() {
        // Splitting on every '+' would leave an empty key name here, which is
        // why the split walks left to right instead.
        let binding = parse("Mod4+Shift+plus=shell zoom.in").expect("should parse");
        assert!(binding.modifiers.logo && binding.modifiers.shift);
        assert_eq!(binding.keysym, keysyms::KEY_plus);
    }

    #[test]
    fn a_shifted_chord_keeps_the_unshifted_key() {
        // "Mod4+Shift+q" is shift plus q, not Q. Storing Q here and comparing
        // against the unmodified keysym at runtime would make every shifted
        // binding unreachable.
        for spec in ["Mod4+Shift+q=close", "Mod4+Shift+h=shell window.move left"] {
            let binding = parse(spec).expect("should parse");
            assert!(binding.modifiers.shift);
            let name = spec.rsplit('+').next().unwrap().split('=').next().unwrap();
            assert_eq!(binding.keysym, keysym_from_name(name).unwrap());
        }
    }

    #[test]
    fn modifiers_are_matched_exactly() {
        // Otherwise Mod4+q fires on Mod4+Shift+q and the shifted binding can
        // never be reached.
        let bindings = vec![parse("Mod4+q=close").unwrap()];
        let plain = ModifiersState {
            logo: true,
            ..Default::default()
        };
        let shifted = ModifiersState {
            logo: true,
            shift: true,
            ..Default::default()
        };
        assert!(match_binding(&bindings, &plain, keysyms::KEY_q).is_some());
        assert!(match_binding(&bindings, &shifted, keysyms::KEY_q).is_none());
    }

    #[test]
    fn exec_keeps_its_whole_command_line() {
        let binding = parse("Mod4+Return=exec foot -e htop").expect("should parse");
        assert_eq!(binding.action, Action::Exec("foot -e htop".to_owned()));
    }

    #[test]
    fn directional_focus_is_the_compositors_own() {
        // `src/binding.c:88` parses "focus <direction>" into an action rather
        // than a passthrough. The shell has no "focus" command at all, so a
        // passthrough is silently dropped with a console warning.
        assert_eq!(
            parse("Mod4+h=focus left").unwrap().action,
            Action::Focus("left".to_owned())
        );
        assert_eq!(
            parse("Mod4+Tab=focus next").unwrap().action,
            Action::Focus("next".to_owned())
        );
        // Still a passthrough when the shell owns it.
        assert_eq!(
            parse("Mod4+h=shell layout.focus left").unwrap().action,
            Action::Shell("layout.focus left".to_owned())
        );
        // A bare "focus" is not a direction.
        assert_eq!(
            parse("Mod4+h=focus").unwrap().action,
            Action::Shell("focus".to_owned())
        );
    }

    #[test]
    fn an_unknown_verb_goes_to_the_shell() {
        // The shell owns layout, so a command this does not implement is not
        // an error — swallowing it here would look like a dead key.
        assert_eq!(
            parse("Mod4+z=layout.something").unwrap().action,
            Action::Shell("layout.something".to_owned())
        );
    }

    #[test]
    fn an_unknown_modifier_is_refused() {
        // Rather than treated as a key name, which would bind something
        // arbitrary.
        assert!(parse("Hyper+q=close").is_none());
        assert!(parse("=close").is_none());
        assert!(parse("Mod4+q=").is_none());
    }

    #[test]
    fn the_defaults_all_parse() {
        // A malformed default is silently dropped by the filter_map, so
        // without this a typo would just remove a binding.
        let bindings = defaults("foot", "wmenu-run", false);
        assert_eq!(bindings.len(), 13 + 16 + 18, "a default failed to parse");

        let scrolling = defaults("foot", "wmenu-run", true);
        assert_eq!(scrolling.len(), bindings.len());
    }

    #[test]
    fn scrolling_sends_focus_to_the_shell_differently() {
        let tiling = defaults("foot", "wmenu-run", false);
        let scrolling = defaults("foot", "wmenu-run", true);
        let find = |bindings: &[Binding]| {
            bindings
                .iter()
                .find(|b| b.keysym == keysyms::KEY_h && !b.modifiers.shift)
                .map(|b| b.action.clone())
        };
        // Tiling: the compositor answers it, because it is a question about
        // where windows are on screen. Sending "focus left" to the shell is
        // what the port did at first and the shell rejects it — there is no
        // such command, only layout.focus.
        assert_eq!(find(&tiling), Some(Action::Focus("left".to_owned())));
        assert_eq!(
            find(&scrolling),
            Some(Action::Shell("layout.focus left".to_owned()))
        );
    }
}
