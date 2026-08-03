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
    /// Enter a binding mode — sway's resize mode and anything a config file
    /// invents.
    ///
    /// A mode is a second keymap: while one is active only its own bindings
    /// match, so `h` can resize there and still move focus everywhere else.
    Mode(String),
    /// Switch the session between dark and light.
    ///
    /// Not the shell's: a client's colour scheme is answered over D-Bus by the
    /// settings portal, which the shell has no way to reach.
    Appearance,
    /// Run the locker now, rather than waiting for `idle.lock_after`.
    ///
    /// The same locker the deadline would run, so there is one place to
    /// configure it and no second answer to what locking means here
    /// (`src/binding.c:614`).
    Lock,
    /// Turn the screens off now, rather than waiting for `idle.blank_after`.
    Blank,
    /// Give the keyboard to the wallpaper terminal on the active monitor, or
    /// take it back.
    ///
    /// The only way input ever reaches it. Everything else about that client
    /// is arranged so that focus cannot land there by accident — it is not a
    /// view, not in the `Space`, not a pointer target — and this is the one
    /// deliberate exception, which is why it is a chord someone has to press
    /// rather than a click on the desktop. See `crate::background`.
    Background,
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
    /// The mode this binding belongs to, empty for the ordinary keymap.
    /// Written `resize/h=...` in a config file.
    pub mode: String,
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

    // A mode prefix, if there is one: `resize/h=...`. Split before the action
    // rather than on the whole line, because an action may contain a slash —
    // `exec /usr/bin/thing` is a binding people write.
    let (mode, chord) = match chord.trim().split_once('/') {
        Some((mode, chord)) => (mode.trim().to_owned(), chord),
        None => (String::new(), chord.trim()),
    };
    if chord.is_empty() {
        return None;
    }

    let binding = parse_chord(chord.trim())?;
    Some(Binding {
        action: parse_action(action),
        mode,
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
        mode: String::new(),
    })
}

fn parse_action(action: &str) -> Action {
    match action.split_once(' ') {
        Some(("exec", rest)) => Action::Exec(rest.trim().to_owned()),
        Some(("shell", rest)) => Action::Shell(rest.trim().to_owned()),
        Some(("focus", rest)) if !rest.trim().is_empty() => Action::Focus(rest.trim().to_owned()),
        Some(("mode", rest)) if !rest.trim().is_empty() => {
            let name = rest.trim();
            // "mode default" leaves whatever mode is active, which is what
            // Escape is bound to inside one.
            Action::Mode(if name == "default" {
                String::new()
            } else {
                name.to_owned()
            })
        }
        _ => match action {
            "close" => Action::Close,
            "exit" => Action::Exit,
            "reload" => Action::Reload,
            "appearance toggle" => Action::Appearance,
            // Both of these are in `defaults` below, so leaving them out did
            // not make them unbound — it made them fall through to the shell,
            // which has no `lock` or `blank` verb and drops what it does not
            // recognise. Two built-in chords that did nothing, silently.
            "lock" => Action::Lock,
            "blank" => Action::Blank,
            "background" => Action::Background,
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
///
/// `layout` names which of the shell's models is running — `"tiling"`,
/// `"scrolling"` or `"solar"` — because a few chords only mean anything in one
/// of them. It is a name rather than a flag per model: three booleans would
/// admit combinations that cannot exist.
pub fn defaults(terminal: &str, menu: &str, layout: &str) -> Vec<Binding> {
    let scrolling = layout == "scrolling";
    let solar = layout == "solar";

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
        "Mod4+e=shell layout.toggle".to_owned(),
        "Mod4+w=shell layout.tabbed".to_owned(),
        "Mod4+s=shell layout.stacked".to_owned(),
        "Mod4+n=shell bar.toggle".to_owned(),
        "Mod4+o=shell layout.overview".to_owned(),
        "Mod4+grave=shell workspace.back".to_owned(),
        // The wallpaper terminal, when there is one. Bound whether or not it
        // is switched on: a chord that reaches nothing is one line in the log
        // rather than a keymap that changes shape with a config key.
        "Mod4+Shift+Return=background".to_owned(),
        "Mod4+Shift+x=lock".to_owned(),
        "Mod4+Shift+b=blank".to_owned(),
        // HDR on the monitor you are looking at rather than all of them: a
        // display that can do it usually sits next to one that cannot.
        "Mod4+Shift+p=shell output.hdr".to_owned(),
        // Media keys, which have no modifier and belong to whatever is
        // playing.
        "XF86AudioPause=exec playerctl pause".to_owned(),
        "XF86AudioNext=exec playerctl next".to_owned(),
        "XF86AudioPrev=exec playerctl previous".to_owned(),
        "XF86AudioStop=exec playerctl stop".to_owned(),
    ];

    if scrolling {
        // niri's column keys. A column is the unit: windows stack inside one
        // and the strip scrolls between them. Consume and expel are what make
        // the model work — pulling the window beside you into your column, or
        // pushing one back out into its own — and a tiling tree has no
        // equivalent.
        specs.push("Mod4+comma=shell layout.consume".to_owned());
        specs.push("Mod4+period=shell layout.expel".to_owned());
        // Cycle the focused column through a few widths, as Mod+R does in
        // niri. Nothing else resizes: columns do not share space, so widening
        // one pushes the rest along the strip rather than taking from a
        // neighbour — which is why this is a cycle and not a mode
        // (`src/binding.c:450`).
        specs.push("Mod4+r=shell layout.column.width".to_owned());
        specs.push("Mod4+Shift+r=shell layout.column.height".to_owned());
        specs.push("Mod4+Home=shell layout.focus first".to_owned());
        specs.push("Mod4+End=shell layout.focus last".to_owned());
    } else if solar {
        // Rotate which window is in which orbital slot, without moving focus
        // and without touching the tree: the gesture for "show me the next
        // few" on a workspace with more windows than the inner orbit holds.
        specs.push("Mod4+bracketright=shell solar.spin 1".to_owned());
        specs.push("Mod4+bracketleft=shell solar.spin -1".to_owned());
        // Throw the focused window at the other monitor, where it arrives as
        // that monitor's centre rather than in a cold corner.
        specs.push("Mod4+Shift+s=shell solar.slingshot".to_owned());
        // The resize gesture. A satellite's size is a function of the middle
        // window's, so there is nothing else to drag — growing the middle one
        // is the only dimension the layout has (`Mod4+r`, the resize mode, is
        // therefore not bound here either).
        specs.push("Mod4+equal=shell solar.mass 1".to_owned());
        specs.push("Mod4+minus=shell solar.mass -1".to_owned());
        // Binary star or Lagrange field: whether the second monitor runs its
        // own system or holds this one's background applications.
        specs.push("Mod4+Shift+g=shell solar.field".to_owned());
    } else {
        // Resize mode, as in sway: Mod4+r enters it, hjkl and the arrows
        // resize a step at a time, Escape or Return leaves. Scoped to the mode
        // so h/j/k/l keep meaning "move focus" everywhere else.
        specs.push("Mod4+r=mode resize".to_owned());
    }

    // The mode's own keymap, which exists whichever layout is running: a
    // config file may bind `mode resize` itself.
    for (key, direction) in [
        ("h", "left"),
        ("j", "down"),
        ("k", "up"),
        ("l", "right"),
        ("Left", "left"),
        ("Down", "down"),
        ("Up", "up"),
        ("Right", "right"),
    ] {
        specs.push(format!("resize/{key}=shell layout.resize {direction}"));
    }
    specs.push("resize/Escape=mode default".to_owned());
    specs.push("resize/Return=mode default".to_owned());
    specs.push("resize/Mod4+r=mode default".to_owned());

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
        // shell can (`src/binding.c:391`). Solar: "the window to the left" of
        // a centre with satellites at four corners is a matter of which corner
        // a ray passes closest to, not of which rectangle shares an edge — so
        // that one goes to the shell too, and by its own verb, because the
        // answer is a ray cast rather than a walk along a strip.
        let focus = if scrolling {
            "shell layout.focus"
        } else if solar {
            "shell solar.ray"
        } else {
            "focus"
        };
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
        specs.push(format!(
            "Mod4+{workspace}=shell workspace.switch {workspace}"
        ));
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
    mode: &str,
) -> Option<&'a Action> {
    let wanted = Modifiers::from_state(modifiers);
    bindings
        .iter()
        .find(|binding| {
            // Only this mode's bindings. A mode is a second keymap rather than
            // an addition to the first: `h` resizes in resize mode and moves
            // focus outside it, and matching both would do whichever came
            // first in the table.
            binding.mode == mode && binding.modifiers == wanted && binding.keysym == keysym
        })
        .map(|binding| &binding.action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_and_blank_are_the_compositors_own() {
        // Neither is the shell's to do: one spawns the configured locker and
        // the other turns the panels off, and the shell can reach neither.
        // Falling through to `Action::Shell` is not a dead key in any visible
        // way — the message goes out, nothing recognises it, nothing happens.
        assert_eq!(parse_action("lock"), Action::Lock);
        assert_eq!(parse_action("blank"), Action::Blank);
    }

    #[test]
    fn the_default_lock_and_blank_chords_are_bound_to_something_real() {
        // `defaults` has always listed both. What it produced was two entries
        // whose action was a shell verb that does not exist.
        let bindings = defaults("foot", "wmenu-run", "tiling");
        assert!(
            bindings.iter().any(|b| b.action == Action::Lock),
            "no binding produces Action::Lock"
        );
        assert!(
            bindings.iter().any(|b| b.action == Action::Blank),
            "no binding produces Action::Blank"
        );
    }

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
        assert!(match_binding(&bindings, &plain, keysyms::KEY_q, "").is_some());
        assert!(match_binding(&bindings, &shifted, keysyms::KEY_q, "").is_none());
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
    fn a_mode_scopes_a_binding() {
        // `h` resizes inside resize mode and moves focus outside it. Matching
        // both would do whichever came first in the table.
        let bindings = vec![
            parse("h=shell layout.focus left").expect("plain"),
            parse("resize/h=shell layout.resize left").expect("scoped"),
        ];
        let plain = ModifiersState::default();

        let outside = match_binding(&bindings, &plain, keysyms::KEY_h, "").expect("outside");
        assert_eq!(outside, &Action::Shell("layout.focus left".to_owned()));

        let inside = match_binding(&bindings, &plain, keysyms::KEY_h, "resize").expect("inside");
        assert_eq!(inside, &Action::Shell("layout.resize left".to_owned()));

        // And a mode with nothing bound in it swallows nothing: the key
        // simply does not match, rather than falling back to the default
        // keymap, which is what makes a mode a mode.
        assert!(match_binding(&bindings, &plain, keysyms::KEY_q, "resize").is_none());
    }

    #[test]
    fn mode_default_leaves_whatever_is_active() {
        // Escape inside a mode is bound to "mode default", and the empty name
        // is what the matcher treats as the ordinary keymap.
        assert_eq!(
            parse("resize/Escape=mode default").expect("parses").action,
            Action::Mode(String::new())
        );
        assert_eq!(
            parse("Mod4+r=mode resize").expect("parses").action,
            Action::Mode("resize".to_owned())
        );
    }

    #[test]
    fn an_action_may_contain_a_slash() {
        // The mode prefix is split off the chord, not the line: `exec
        // /usr/bin/thing` is a binding people write, and splitting the whole
        // line would make "Mod4+t=exec " a binding in a mode called
        // "Mod4+t=exec /usr".
        let binding = parse("Mod4+t=exec /usr/bin/foot").expect("parses");
        assert_eq!(binding.mode, "");
        assert_eq!(binding.action, Action::Exec("/usr/bin/foot".to_owned()));
    }

    #[test]
    fn the_defaults_all_parse() {
        // A malformed default is silently dropped by the filter_map, so
        // without this a typo would just remove a binding.
        // 27 plain, 16 directional, 18 workspace, 11 in resize mode, and one
        // more that enters it.
        let bindings = defaults("foot", "wmenu-run", "tiling");
        assert_eq!(
            bindings.len(),
            27 + 16 + 18 + 11 + 1,
            "a default failed to parse"
        );

        // Scrolling has no resize mode to enter — a column does not share
        // space with its neighbours — and six column bindings instead.
        let scrolling = defaults("foot", "wmenu-run", "scrolling");
        assert_eq!(scrolling.len(), bindings.len() - 1 + 6);

        // Solar has no resize mode either, for the same kind of reason: a
        // satellite's size is a function of the middle window's, so the only
        // thing to resize is that one. Six of its own in place of it.
        let solar = defaults("foot", "wmenu-run", "solar");
        assert_eq!(solar.len(), bindings.len() - 1 + 6);

        // An unknown layout is the tiling keymap rather than a keyboard with
        // holes in it: the name reaches here unvalidated, because the
        // compositor has no layout and cannot judge one.
        assert_eq!(
            defaults("foot", "wmenu-run", "orbital").len(),
            bindings.len()
        );
    }

    #[test]
    fn scrolling_sends_focus_to_the_shell_differently() {
        let tiling = defaults("foot", "wmenu-run", "tiling");
        let scrolling = defaults("foot", "wmenu-run", "scrolling");
        let find = |bindings: &[Binding]| {
            bindings
                .iter()
                // The ordinary keymap: resize mode binds h as well, and it is
                // a different question with a different answer.
                .find(|b| b.mode.is_empty() && b.keysym == keysyms::KEY_h && !b.modifiers.shift)
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
        // Solar: a third answer, and a third verb. It is neither a walk along
        // a strip nor a rectangle comparison — the shell casts a ray from the
        // middle window — so reusing either spelling would send it to code
        // that would answer the wrong question convincingly.
        let solar = defaults("foot", "wmenu-run", "solar");
        assert_eq!(
            find(&solar),
            Some(Action::Shell("solar.ray left".to_owned()))
        );
    }
}
