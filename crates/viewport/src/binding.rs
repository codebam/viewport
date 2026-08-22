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
    /// Zoom the screen in, out, or straight back to 1:1.
    ///
    /// The screen magnifier, and not `canvas.zoom`: that one is a layout the
    /// shell draws at a scale, and its own header records that input only
    /// lands where it is aimed at 1.0 — which is the opposite of what
    /// magnifying a screen is for. This is a compositor action because it is a
    /// property of the real output: the region is composited larger and the
    /// pointer is not moved at all, so nothing about it is expressible as a
    /// message to a page that is itself one of the things being magnified.
    /// See [`crate::magnify`].
    Magnify(crate::magnify::Step),
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
    /// The xkb symbol of the key, when this is a keyboard binding.
    pub keysym: u32,
    /// The libinput button code, when this is a mouse binding (`Mouse4`,
    /// `BTN_LEFT`, ...). A binding is one or the other: a chord is drawn on a
    /// key or on a button, never both.
    pub button: Option<u32>,
    /// The direction of a scroll-wheel binding (`WheelUp`/`WheelDown`).
    pub wheel: Option<Wheel>,
    pub action: Action,
    /// The mode this binding belongs to, empty for the ordinary keymap.
    /// Written `resize/h=...` in a config file.
    pub mode: String,
}

/// The direction a scroll-wheel binding matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wheel {
    /// Away from the hand — a conventional wheel's "up".
    Up,
    /// Toward the hand.
    Down,
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

impl Binding {
    /// This chord, spelled the way a config file spells it.
    ///
    /// Round-trips through `parse_chord`: what comes out here can be typed
    /// back in. That is the property worth having — the shell shows these to
    /// someone who may want to rebind one, and a listing written in a notation
    /// the parser does not accept is a listing that has to be translated by
    /// hand before it is any use.
    pub fn chord(&self) -> String {
        let mut chord = String::new();
        // The order every default is written in, so a listing and the config
        // file it came from read the same way round.
        if self.modifiers.logo {
            chord.push_str("Mod4+");
        }
        if self.modifiers.ctrl {
            chord.push_str("Ctrl+");
        }
        if self.modifiers.alt {
            chord.push_str("Alt+");
        }
        if self.modifiers.shift {
            chord.push_str("Shift+");
        }

        match (self.button, self.wheel) {
            (Some(button), _) => chord.push_str(&button_name(button)),
            (None, Some(Wheel::Up)) => chord.push_str("WheelUp"),
            (None, Some(Wheel::Down)) => chord.push_str("WheelDown"),
            (None, None) => chord.push_str(&smithay::input::keyboard::xkb::keysym_get_name(
                smithay::input::keyboard::Keysym::new(self.keysym),
            )),
        }
        chord
    }

    /// What this chord does, spelled the way a config file spells it.
    pub fn action_text(&self) -> String {
        match &self.action {
            Action::Exec(command) => format!("exec {command}"),
            Action::Close => "close".to_owned(),
            Action::Exit => "exit".to_owned(),
            Action::Focus(direction) => format!("focus {direction}"),
            Action::Reload => "reload".to_owned(),
            Action::Mode(name) if name.is_empty() => "mode default".to_owned(),
            Action::Mode(name) => format!("mode {name}"),
            Action::Appearance => "appearance toggle".to_owned(),
            Action::Lock => "lock".to_owned(),
            Action::Blank => "blank".to_owned(),
            Action::Background => "background".to_owned(),
            Action::Magnify(crate::magnify::Step::In) => "magnify in".to_owned(),
            Action::Magnify(crate::magnify::Step::Out) => "magnify out".to_owned(),
            Action::Magnify(crate::magnify::Step::Off) => "magnify off".to_owned(),
            Action::Shell(command) => format!("shell {command}"),
        }
    }
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

    let (keysym, button, wheel) = if let Some(wheel) = wheel_from_name(rest) {
        // Scroll wheel: not a key and not a button, but the same chord. Its
        // modifier prefix is the only part it shares with either.
        (0, None, Some(wheel))
    } else if let Some(keysym) = keysym_from_name(rest) {
        // A key the keymap knows wins over a button spelled the same way.
        //
        // The button names overlap the keysyms: `Left` and `Right` are the
        // arrow keys and also two of the spellings for the buttons beside the
        // wheel. Asking the button table first made `Mod4+Left=focus left` —
        // a default, and in every config written before buttons could be
        // bound at all — a binding on Mod4+left-click, which then swallowed
        // every Mod4-held click before it reached the client under it. On an
        // `auto` bar, which is only on screen while Mod4 is held, that is
        // every click a bar widget could ever get; the scroll wheel kept
        // working, because nothing was bound to it, which is what the fault
        // looked like from the outside.
        (keysym, None, None)
    } else {
        // A mouse button is its own kind of key, with no keysym to match —
        // the modifier prefix is the only part it shares with a chord.
        (0, Some(button_from_name(rest)?), None)
    };
    Some(Binding {
        modifiers,
        keysym,
        button,
        wheel,
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
            // Spelled out rather than parsed as a verb with an argument, so
            // that a typo — `magnify up` — falls through to the shell and is
            // one ignored message, instead of matching here and silently
            // becoming one of the three.
            "magnify in" => Action::Magnify(crate::magnify::Step::In),
            "magnify out" => Action::Magnify(crate::magnify::Step::Out),
            "magnify off" => Action::Magnify(crate::magnify::Step::Off),
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

/// An evdev button code by name.
///
/// libinput numbers the side buttons `BTN_SIDE` (0x113) and `BTN_EXTRA`
/// (0x114); everyone else calls them Mouse4 and Mouse5, or XButton1 and
/// XButton2. All three spellings are accepted, case-insensitively.
///
/// Asked only for a name the keymap does not know: `left` and `right` are
/// listed here and are also the arrow keys, and a chord that could be either
/// is the key — see `parse_chord`.
/// The name a button is written by, for showing a binding back to someone.
///
/// The first spelling `button_from_name` accepts, so what is displayed is
/// something that can be typed straight back into a config file.
fn button_name(button: u32) -> String {
    match button {
        0x110 => "Mouse1".to_owned(),
        0x112 => "Mouse2".to_owned(),
        0x111 => "Mouse3".to_owned(),
        0x113 => "Mouse4".to_owned(),
        0x114 => "Mouse5".to_owned(),
        other => format!("Button{other}"),
    }
}

fn button_from_name(name: &str) -> Option<u32> {
    let button = match name.to_ascii_lowercase().as_str() {
        "btn_left" | "mouse1" | "left" | "button1" => 0x110,
        "btn_middle" | "mouse2" | "middle" | "button2" => 0x112,
        "btn_right" | "mouse3" | "right" | "button3" => 0x111,
        "btn_side" | "mouse4" | "xbutton1" | "button4" => 0x113,
        "btn_extra" | "mouse5" | "xbutton2" | "button5" => 0x114,
        _ => return None,
    };
    Some(button)
}

/// A scroll-wheel direction by name.
///
/// The wheel is not a button in libinput's terms — it arrives as an axis, not
/// a press — so it is its own kind of binding, written `WheelUp`/`WheelDown`
/// (also `ScrollUp`/`ScrollDown`).
fn wheel_from_name(name: &str) -> Option<Wheel> {
    match name.to_ascii_lowercase().as_str() {
        "wheelup" | "scrollup" => Some(Wheel::Up),
        "wheeldown" | "scrolldown" => Some(Wheel::Down),
        _ => None,
    }
}

/// The bindings a session starts with. Ports `add_default` in src/binding.c.
///
/// Layout verbs are all passthroughs: the shell decides what splitting,
/// fullscreen and moving mean, and duplicating that judgement here would give
/// two things an opinion about it.
///
/// `layout` names which of the shell's models is running — `"tiling"`,
/// `"scrolling"`, `"solar"`, `"matrix"` or `"canvas"` — because a few chords
/// only mean anything in one of them. It is a name rather than a flag per
/// model: a boolean each would admit combinations that cannot exist.
///
/// `menu` is the external menu command, when one is named. `None` — the key
/// left out of the config file and the variable unset — is the built-in
/// launcher, which the shell draws and the compositor feeds: `Mod4+d` goes to
/// the shell as a verb rather than to a process with its own theme and no
/// idea which monitor asked for it.
pub fn defaults(terminal: &str, menu: Option<&str>, layout: &str) -> Vec<Binding> {
    let scrolling = layout == "scrolling";
    let solar = layout == "solar";
    let canvas = layout == "canvas";

    let mut specs: Vec<String> = vec![
        format!("Mod4+Return=exec {terminal}"),
        match menu {
            Some(menu) => format!("Mod4+d=exec {menu}"),
            None => "Mod4+d=shell launcher".to_owned(),
        },
        "Mod4+Shift+q=close".to_owned(),
        "Mod4+Shift+e=exit".to_owned(),
        "Mod4+Shift+c=reload".to_owned(),
        "Mod4+Shift+d=appearance toggle".to_owned(),
        // The clipboard history, drawn by the shell. Shift+v beside the paste
        // everyone already knows, and a shell command rather than a compositor
        // action because what a picker is belongs to the page.
        "Mod4+Shift+v=shell clipboard".to_owned(),
        // The notification centre — the record of what was notified while
        // nobody was looking. `m` for message, which nothing else on this
        // modifier has claimed, and a shell verb for the reason the clipboard
        // picker is one: the compositor keeps the list and the page draws it.
        "Mod4+Shift+m=shell notifications".to_owned(),
        // The two radios, on the same terms as the clipboard picker above: the
        // compositor does the talking to NetworkManager and BlueZ, because the
        // page has no bus, and the shell draws the list — so these are shell
        // verbs rather than compositor actions.
        //
        // `n` and `t` rather than anything mnemonic for Bluetooth: `Mod4+n`
        // already toggles the bar, so the network sits beside it under Shift,
        // and `b` is taken by the horizontal split with `Mod4+Shift+b`
        // blanking the screens. `t` is what is left that nothing else wants.
        "Mod4+Shift+n=shell network".to_owned(),
        "Mod4+Shift+t=shell bluetooth".to_owned(),
        // The on-screen keyboard. It also comes up on its own — see
        // `osk.wanted` in ipc.md — so this is only for the desk that has no
        // hardware keyboard to press it with in the first place; it exists
        // for the desk that does, where the desktop wants to bring it up to
        // test it or to type into something that never asked. `k` for
        // keyboard, which nothing else on this modifier has claimed.
        "Mod4+Shift+k=shell osk".to_owned(),
        // The settings panel, on the same terms as the pickers above: the
        // compositor holds the settings and the page draws the switches, so
        // this is a shell verb rather than a compositor action.
        //
        // Comma because a comma is what a settings shortcut is nearly
        // everywhere else, and because the letters are gone: every one that
        // means anything here is already a split, a layout or a picker.
        // `Mod4+comma` itself is the scrolling layout's consume, so the panel
        // sits beside it under Shift, where nothing else is.
        "Mod4+Shift+comma=shell settings".to_owned(),
        // Tab is filled in below: two layouts have windows the compositor
        // cannot cycle through, so the chord goes to the shell there.
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
        // The screen magnifier. On Alt rather than on Shift because the two
        // obvious keys are already spoken for twice over: `Mod4+equal` and
        // `Mod4+minus` grow a solar system's middle window under one layout
        // and zoom the canvas under another, and a chord whose meaning
        // depended on the layout would be the one chord that has to work when
        // somebody cannot read the screen well enough to know which layout is
        // up. `Mod4+Alt+0` is the way back to 1:1, and it is worth a key of
        // its own for the reason `canvas.home` is: at 8x, finding the
        // zoom-out key means finding it through the part of the screen that
        // is on it.
        "Mod4+Alt+equal=magnify in".to_owned(),
        "Mod4+Alt+minus=magnify out".to_owned(),
        "Mod4+Alt+0=magnify off".to_owned(),
        // HDR on the monitor you are looking at rather than all of them: a
        // display that can do it usually sits next to one that cannot.
        "Mod4+Shift+p=shell output.hdr".to_owned(),
        // Media keys, which have no modifier and belong to whatever is
        // playing.
        //
        // The play key is `XF86AudioPlay`, and it is the one key on the row
        // that has to be named exactly right. A keyboard's play/pause key is
        // `KEY_PLAYPAUSE`, which xkb maps to `[XF86AudioPlay,
        // XF86AudioPause]` — the pause name is the *shifted* level of that
        // key, and chords are matched on the unshifted keysym, so binding
        // `XF86AudioPause` alone bound a level nothing can reach. Skip and
        // previous worked, because their keysyms are the ones their keys
        // actually send, and the odd one out looked like playerctl failing
        // rather than a chord that was never matched.
        //
        // `play-pause` rather than `pause` for the same reason: one key, one
        // toggle. A player already paused is started again by the key that
        // paused it, which is what the key is labelled.
        "XF86AudioPlay=exec playerctl play-pause".to_owned(),
        // The dedicated pause key, which is `KEY_PAUSECD` and a different key
        // — rare on a keyboard, present on some remotes and media decks. It
        // pauses rather than toggling: a key that says pause has said what it
        // means.
        "XF86AudioPause=exec playerctl pause".to_owned(),
        "XF86AudioNext=exec playerctl next".to_owned(),
        "XF86AudioPrev=exec playerctl previous".to_owned(),
        "XF86AudioStop=exec playerctl stop".to_owned(),
        // The rest of the row, which docs/configuration.md has described as
        // bound by default since it was written and which nothing bound: they
        // were in `data/config.example.json` only, so a desktop that had not
        // copied that file had a volume key that did nothing.
        //
        // The sink rather than the player, deliberately — turning the volume
        // down means the machine, not whatever happens to be playing — which
        // is why these go through `wpctl` and the four above through MPRIS.
        //
        // Five percent a press, because a binding fires on press and does not
        // repeat while the key is held: the one percent the example file
        // suggests is a fine adjustment, and a hundred presses from silence to
        // full is not a volume key.
        "XF86AudioRaiseVolume=exec wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+".to_owned(),
        "XF86AudioLowerVolume=exec wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-".to_owned(),
        "XF86AudioMute=exec wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle".to_owned(),
        // The microphone's own key, which is the source and not the sink. On
        // the keyboards that have it, it is the key a video call is muted with.
        "XF86AudioMicMute=exec wpctl set-mute @DEFAULT_AUDIO_SOURCE@ toggle".to_owned(),
        "XF86MonBrightnessUp=exec brightnessctl set 5%+".to_owned(),
        "XF86MonBrightnessDown=exec brightnessctl set 5%-".to_owned(),
    ];

    // Step through the windows, and *which* windows depends on who can see
    // them all.
    //
    // The compositor's own cycle walks the windows that are on screen, which
    // is every window the tiling, solar and matrix layouts have. The scrolling
    // strip and the canvas both keep windows outside the view — a column
    // scrolled past the edge, a window panned off the plane — and report them
    // to the compositor as not on screen, because that is what stops their
    // surfaces being painted into holes that are not there. Cycling from here
    // could then reach the neighbours of the view and nothing beyond them,
    // which is the one key whose job is reaching them.
    //
    // So in those two the chord goes to the shell, exactly as directional
    // focus does in the strip and for the same reason. See `focus_direction`
    // and `scrollFocus`.
    if scrolling || canvas {
        specs.push("Mod4+Tab=shell layout.focus next".to_owned());
        specs.push("Mod4+Shift+Tab=shell layout.focus prev".to_owned());
    } else {
        specs.push("Mod4+Tab=focus next".to_owned());
        specs.push("Mod4+Shift+Tab=focus prev".to_owned());
    }

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
    } else if canvas {
        // Move the view over the plane. Bound to the bracket keys and the
        // page keys rather than to hjkl, which keep meaning "move focus":
        // panning is what you do *between* windows and focus is what you do
        // to them, and a layout where the two shared a chord would make
        // reaching the window beside you depend on how far the view had
        // drifted.
        specs.push("Mod4+bracketleft=shell canvas.pan left".to_owned());
        specs.push("Mod4+bracketright=shell canvas.pan right".to_owned());
        specs.push("Mod4+Prior=shell canvas.pan up".to_owned());
        specs.push("Mod4+Next=shell canvas.pan down".to_owned());
        // Out and in. Zoom stops at 1.0 by design — see the header of
        // data/shell/canvas.js — so `equal` runs out rather than overshooting
        // into a scale the compositor's hit test cannot follow.
        specs.push("Mod4+minus=shell canvas.zoom out".to_owned());
        specs.push("Mod4+equal=shell canvas.zoom in".to_owned());
        // The whole plane, and back to 1:1 on what is focused. The second is
        // the one that matters: 1.0 is the only zoom at which a click reaches
        // the pixel it appears to, so "let me use this again" needs a key and
        // not four presses of zoom-in.
        specs.push("Mod4+Shift+f=shell canvas.fit".to_owned());
        specs.push("Mod4+Home=shell canvas.home".to_owned());
        // The resize chord every other layout spends on a mode. There is no
        // mode to enter here — a window on a plane takes space from nothing —
        // so the one resize worth a key is the one that is tedious by hand:
        // fill the screen. Not fullscreen, which is Mod4+f and takes the
        // output; this leaves an ordinary window that happens to be screen
        // sized, with the plane still behind it.
        specs.push("Mod4+r=shell canvas.fill".to_owned());
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
            // A key binding and not a mouse one. A button or wheel binding
            // carries `keysym: 0`, and an unmapped keycode produces exactly
            // that — so without this every NoSymbol press fired whatever
            // `Mod4+Mouse4` was bound to.
            binding.mode == mode
                && binding.modifiers == wanted
                && binding.keysym == keysym
                && binding.button.is_none()
                && binding.wheel.is_none()
        })
        .map(|binding| &binding.action)
}

/// The action a pressed mouse button fires, if any.
///
/// The same matcher as [`match_binding`], but for a button and not a key:
/// modifiers are compared against the keyboard's state, and a binding whose
/// `button` is `None` (a chord) can never match a button.
pub fn match_button<'a>(
    bindings: &'a [Binding],
    modifiers: &ModifiersState,
    button: u32,
    mode: &str,
) -> Option<&'a Action> {
    let wanted = Modifiers::from_state(modifiers);
    bindings
        .iter()
        .find(|binding| {
            binding.mode == mode && binding.modifiers == wanted && binding.button == Some(button)
        })
        .map(|binding| &binding.action)
}

/// The action a scroll up or down fires, if any.
///
/// The same matcher as [`match_button`], but for a wheel direction. A binding
/// whose `wheel` is `None` (a chord or a button) can never match a scroll.
pub fn match_wheel<'a>(
    bindings: &'a [Binding],
    modifiers: &ModifiersState,
    wheel: Wheel,
    mode: &str,
) -> Option<&'a Action> {
    let wanted = Modifiers::from_state(modifiers);
    bindings
        .iter()
        .find(|binding| {
            binding.mode == mode && binding.modifiers == wanted && binding.wheel == Some(wheel)
        })
        .map(|binding| &binding.action)
}

/// The chord that always leaves, added when nothing else does.
///
/// Every other binding is the config file's to decide, and this one nearly is:
/// `binds` replaces the defaults outright, and `binds_override` can unbind a
/// chord with a `null`. Either can produce a session with no way out of it —
/// docs/configuration.md warns about exactly that — and the warning is only
/// any use to somebody who read it before rebooting into the session they
/// wrote.
///
/// It matters most where there is no shell to fall back on. `--url
/// https://example.com` is a web page and nothing else: no launcher, no
/// terminal binding it can reach, nothing on screen that quits. Mod4+Shift+E
/// is then the whole interface, and a config file that happened to drop it
/// leaves a machine that has to be held down by the power button.
///
/// Added last, so it loses to anything the file did bind to that chord — this
/// is a floor, not an override. `Ctrl+Alt+Backspace` is still there underneath
/// as the chord no config file can touch (`crate::input::shortcut`); this is
/// the one people are told about.
/// Whether `earlier` would match everything `binding` matches, and so — being
/// earlier in the list — leave it unreachable. The same test `match_binding`
/// makes, written once.
fn shadows(earlier: &Binding, binding: &Binding) -> bool {
    earlier.mode == binding.mode
        && earlier.modifiers == binding.modifiers
        && earlier.keysym == binding.keysym
        && earlier.button.is_none()
        && earlier.wheel.is_none()
}

pub fn guarantee_an_exit(bindings: &mut Vec<Binding>) {
    // Reachable, not merely present. Bindings are matched first-wins and
    // `binds_override` is pushed in *front* of the defaults, so
    // `{"binds_override": {"Mod4+Shift+e": null}}` leaves the default exit in
    // the list with an unbind standing in front of it — a way out that no key
    // reaches, which is the exact case this function exists for.
    let leaves = bindings.iter().enumerate().any(|(at, binding)| {
        binding.mode.is_empty()
            && binding.action == Action::Exit
            && binding.button.is_none()
            && binding.wheel.is_none()
            && !bindings[..at]
                .iter()
                .any(|earlier| shadows(earlier, binding))
    });
    if leaves {
        return;
    }
    let Some(exit) = parse("Mod4+Shift+e=exit") else {
        // Unreachable short of the parser losing `exit`, and a panic here
        // would be a compositor that will not start over a keybinding.
        tracing::error!("could not build the fallback exit binding");
        return;
    };
    // Only if the chord itself is free. A file that bound Mod4+Shift+E to
    // something else meant it, and quietly doing something different from what
    // it says is worse than the risk this exists to cover. Appended, so
    // anything already on that chord would match first anyway.
    if bindings.iter().any(|binding| shadows(binding, &exit)) {
        tracing::warn!(
            "no binding leaves this session and Mod4+Shift+E is taken; \
             Ctrl+Alt+Backspace is the way out"
        );
        return;
    }
    tracing::info!("no binding leaves this session; adding Mod4+Shift+e=exit");
    bindings.push(exit);
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
        let bindings = defaults("foot", Some("wmenu-run"), "tiling");
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
    fn the_magnifier_chords_are_the_compositors_own() {
        use crate::magnify::Step;
        // Not the shell's: the shell is a page that is itself being
        // magnified, and it has no way to composite the output it is drawn
        // into. Falling through to `Action::Shell` would be three chords that
        // send a message nothing recognises — which is not a dead key in any
        // visible way, which is exactly how `lock` and `blank` were broken.
        assert_eq!(parse_action("magnify in"), Action::Magnify(Step::In));
        assert_eq!(parse_action("magnify out"), Action::Magnify(Step::Out));
        assert_eq!(parse_action("magnify off"), Action::Magnify(Step::Off));
        // And a fourth verb is not quietly one of the three.
        assert_eq!(
            parse_action("magnify up"),
            Action::Shell("magnify up".to_owned())
        );

        for layout in ["tiling", "scrolling", "solar", "matrix", "canvas"] {
            let bindings = defaults("foot", Some("wmenu-run"), layout);
            for step in [Step::In, Step::Out, Step::Off] {
                assert!(
                    bindings.iter().any(|b| b.action == Action::Magnify(step)),
                    "no binding produces Action::Magnify({step:?}) under {layout}"
                );
            }
        }
    }

    /// Every default chord round-trips through the text a config file writes.
    ///
    /// The magnifier is the reason this is worth restating: its action_text is
    /// three separate strings rather than one format, so a fourth step added
    /// without a fourth arm would spell as something `parse_action` sends to
    /// the shell — a chord the keymap on an empty desktop shows and pressing
    /// which does nothing.
    #[test]
    fn a_magnifier_binding_spells_back_to_itself() {
        use crate::magnify::Step;
        for step in [Step::In, Step::Out, Step::Off] {
            let action = Action::Magnify(step);
            let binding = Binding {
                modifiers: Modifiers::default(),
                keysym: 0,
                button: None,
                wheel: None,
                action: action.clone(),
                mode: String::new(),
            };
            assert_eq!(parse_action(&binding.action_text()), action);
        }
    }

    /// The play/pause key is bound to the keysym that key actually sends.
    ///
    /// `KEY_PLAYPAUSE` is `[XF86AudioPlay, XF86AudioPause]` in xkb's evdev
    /// map, and chords are matched on the unshifted keysym — so a keymap
    /// carrying only `XF86AudioPause` binds a level the key cannot produce.
    /// Skip and previous went on working, which made the one dead key look
    /// like a player that would not answer.
    #[test]
    fn the_play_key_is_bound_to_the_keysym_it_sends() {
        let bindings = defaults("foot", Some("wmenu-run"), "tiling");
        let action = |keysym| {
            bindings
                .iter()
                .find(|b| b.mode.is_empty() && b.keysym == keysym)
                .map(|b| b.action.clone())
        };

        assert_eq!(
            action(keysyms::KEY_XF86AudioPlay),
            Some(Action::Exec("playerctl play-pause".to_owned())),
            "the key labelled play/pause is XF86AudioPlay, and it toggles"
        );
        // The rest of the row, so a rename that fixes one and breaks another
        // is caught here rather than by pressing them.
        assert_eq!(
            action(keysyms::KEY_XF86AudioNext),
            Some(Action::Exec("playerctl next".to_owned()))
        );
        assert_eq!(
            action(keysyms::KEY_XF86AudioPrev),
            Some(Action::Exec("playerctl previous".to_owned()))
        );
        assert_eq!(
            action(keysyms::KEY_XF86AudioStop),
            Some(Action::Exec("playerctl stop".to_owned()))
        );
        // And the dedicated pause key, which is a different key: it pauses.
        assert_eq!(
            action(keysyms::KEY_XF86AudioPause),
            Some(Action::Exec("playerctl pause".to_owned()))
        );

        // The volume and brightness keys, which docs/configuration.md has
        // described as bound by default since it was written and which lived
        // in the example config alone — so a desktop that had not copied that
        // file had a volume key that did nothing.
        for keysym in [
            keysyms::KEY_XF86AudioRaiseVolume,
            keysyms::KEY_XF86AudioLowerVolume,
            keysyms::KEY_XF86AudioMute,
            keysyms::KEY_XF86AudioMicMute,
            keysyms::KEY_XF86MonBrightnessUp,
            keysyms::KEY_XF86MonBrightnessDown,
        ] {
            assert!(
                matches!(action(keysym), Some(Action::Exec(_))),
                "{} runs nothing",
                smithay::input::keyboard::xkb::keysym_get_name(
                    smithay::input::keyboard::Keysym::new(keysym)
                )
            );
        }
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
    fn a_mouse_button_parses_into_a_button_not_a_keysym() {
        // Mouse4/Mouse5 are libinput's BTN_SIDE and BTN_EXTRA, and a config
        // names them the way people do.
        let binding = parse("Mod4+Mouse4=shell workspace.switch 1").expect("should parse");
        assert!(binding.modifiers.logo);
        assert_eq!(binding.button, Some(0x113));
        assert_eq!(binding.keysym, 0, "a button is not a key");
        assert_eq!(
            binding.action,
            Action::Shell("workspace.switch 1".to_owned())
        );
    }

    #[test]
    fn a_button_binding_is_matched_by_button_and_modifier() {
        let bindings = vec![parse("Mod4+Mouse4=close").unwrap()];
        let held = ModifiersState {
            logo: true,
            ..Default::default()
        };
        let released = ModifiersState::default();
        assert_eq!(
            match_button(&bindings, &held, 0x113, ""),
            Some(&Action::Close)
        );
        // Not Mouse5, and not without the modifier.
        assert!(match_button(&bindings, &held, 0x114, "").is_none());
        assert!(match_button(&bindings, &released, 0x113, "").is_none());
    }

    #[test]
    fn a_button_never_matches_a_key_and_vice_versa() {
        let key = parse("Mod4+q=close").unwrap();
        let button = parse("Mod4+Mouse4=close").unwrap();
        let held = ModifiersState {
            logo: true,
            ..Default::default()
        };
        // The key binding has no button, so no button can match it.
        assert!(match_button(std::slice::from_ref(&key), &held, 0x113, "").is_none());
        // And the button binding has keysym 0, not q.
        assert!(match_binding(&[button], &held, keysyms::KEY_q, "").is_none());
    }

    #[test]
    fn an_unmapped_key_does_not_fire_a_mouse_binding() {
        // An unmapped keycode comes through as NoSymbol, which is keysym 0 —
        // and 0 is exactly what a button or wheel binding carries, because it
        // is drawn on no key at all. Matched on the keysym alone, every press
        // of a key xkb has no symbol for fired whatever `Mod4+Mouse4` was
        // bound to.
        let bindings = vec![
            parse("Mod4+Mouse4=close").unwrap(),
            parse("Mod4+WheelUp=exit").unwrap(),
        ];
        let held = ModifiersState {
            logo: true,
            ..Default::default()
        };
        assert!(match_binding(&bindings, &held, 0, "").is_none());
    }

    #[test]
    fn an_arrow_key_is_a_key_and_not_the_button_of_the_same_name() {
        // `Left` and `Right` name both an arrow key and a mouse button, and
        // the key is what a config means: `Mod4+Left=focus left` is a default.
        // Read as a button it bound Mod4+left-click, and the button handler
        // consumes what it matches — so every Mod4-held click was swallowed
        // before the client under the pointer saw it. The `auto` bar is only
        // on screen while Mod4 is held, which made its widgets unclickable.
        for name in ["Left", "Right", "left", "right"] {
            let binding =
                parse(&format!("Mod4+{name}=focus {}", name.to_lowercase())).expect("should parse");
            assert_eq!(binding.button, None, "{name} is a key, not a button");
            assert_eq!(binding.keysym, keysym_from_name(name).unwrap());
        }

        // And nothing in the default keymap claims a plain or a modified
        // left/right click, which is what leaves the shell's own widgets
        // clickable.
        let bindings = defaults("foot", Some("fuzzel"), "scrolling");
        let held = ModifiersState {
            logo: true,
            ..Default::default()
        };
        for modifiers in [ModifiersState::default(), held] {
            for button in [0x110, 0x111] {
                assert!(match_button(&bindings, &modifiers, button, "").is_none());
            }
        }
    }

    #[test]
    fn a_wheel_parses_into_a_direction_not_a_key() {
        let up = parse("Mod4+WheelUp=shell workspace.next").expect("should parse");
        assert!(up.modifiers.logo);
        assert_eq!(up.wheel, Some(Wheel::Up));
        assert_eq!(up.button, None);
        assert_eq!(up.keysym, 0, "the wheel is not a key");

        let down = parse("Mod4+ScrollDown=shell workspace.prev").expect("should parse");
        assert_eq!(down.wheel, Some(Wheel::Down));
    }

    #[test]
    fn a_wheel_is_matched_by_direction_and_modifier() {
        let bindings = vec![parse("Mod4+WheelUp=close").unwrap()];
        let held = ModifiersState {
            logo: true,
            ..Default::default()
        };
        let released = ModifiersState::default();
        assert_eq!(
            match_wheel(&bindings, &held, Wheel::Up, ""),
            Some(&Action::Close)
        );
        // Not the other direction, and not without the modifier.
        assert!(match_wheel(&bindings, &held, Wheel::Down, "").is_none());
        assert!(match_wheel(&bindings, &released, Wheel::Up, "").is_none());
        // Nor does a button or a key bind a wheel.
        let key = parse("Mod4+q=close").unwrap();
        let button = parse("Mod4+Mouse4=close").unwrap();
        assert!(match_wheel(&[key, button], &held, Wheel::Up, "").is_none());
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
        // 43 plain, 16 directional, 18 workspace, 11 in resize mode, and one
        // more that enters it.
        let bindings = defaults("foot", Some("wmenu-run"), "tiling");
        assert_eq!(
            bindings.len(),
            43 + 16 + 18 + 11 + 1,
            "a default failed to parse"
        );

        // Scrolling has no resize mode to enter — a column does not share
        // space with its neighbours — and six column bindings instead.
        let scrolling = defaults("foot", Some("wmenu-run"), "scrolling");
        assert_eq!(scrolling.len(), bindings.len() - 1 + 6);

        // Solar has no resize mode either, for the same kind of reason: a
        // satellite's size is a function of the middle window's, so the only
        // thing to resize is that one. Six of its own in place of it.
        let solar = defaults("foot", Some("wmenu-run"), "solar");
        assert_eq!(solar.len(), bindings.len() - 1 + 6);

        // The canvas has no resize mode either — a window on a plane is sized
        // by dragging it, not by taking space from a neighbour it does not
        // share any with — and nine of its own in place of it, one of which
        // takes over `Mod4+r` from the mode.
        let canvas = defaults("foot", Some("wmenu-run"), "canvas");
        assert_eq!(canvas.len(), bindings.len() - 1 + 9);

        // And every one of those nine is really there, by name. The count
        // above would be satisfied by nine of anything, and the failure this
        // is written against is a keysym the parser does not know: `filter_map`
        // drops the whole binding, so a chord that does nothing looks exactly
        // like a layout that does not respond to the keyboard.
        for chord in [
            "Mod4+bracketleft",
            "Mod4+bracketright",
            "Mod4+Prior",
            "Mod4+Next",
            "Mod4+minus",
            "Mod4+equal",
            "Mod4+Shift+f",
            "Mod4+Home",
            "Mod4+r",
        ] {
            let wanted = parse_chord(chord).expect("the test's own chord parses");
            assert!(
                canvas.iter().any(|binding| binding.mode.is_empty()
                    && binding.keysym == wanted.keysym
                    && binding.modifiers == wanted.modifiers),
                "the canvas keymap is missing {chord}"
            );
        }

        // An unknown layout is the tiling keymap rather than a keyboard with
        // holes in it: the name reaches here unvalidated, because the
        // compositor has no layout and cannot judge one.
        assert_eq!(
            defaults("foot", Some("wmenu-run"), "orbital").len(),
            bindings.len()
        );
    }

    /// The two radio pickers are bound, and bound to the shell.
    ///
    /// Both are verbs the shell answers — the compositor holds the bus
    /// connection and the page draws the list — so a default that reached
    /// `Action::Exec` or an unparsed chord would be a key that does nothing
    /// visible: `filter_map` drops a binding it cannot read, and a shell verb
    /// nothing recognises is a message that goes out and is ignored. Neither
    /// failure is one anybody would notice without pressing the key.
    #[test]
    fn the_radio_pickers_are_bound_to_shell_verbs() {
        let bindings = defaults("foot", Some("wmenu-run"), "tiling");
        for (chord, verb) in [("Mod4+Shift+n", "network"), ("Mod4+Shift+t", "bluetooth")] {
            let wanted = parse_chord(chord).expect("the test's own chord parses");
            let found = bindings.iter().find(|binding| {
                binding.mode.is_empty()
                    && binding.keysym == wanted.keysym
                    && binding.modifiers == wanted.modifiers
            });
            assert_eq!(
                found.map(|binding| binding.action.clone()),
                Some(Action::Shell(verb.to_owned())),
                "{chord} should open the {verb} picker"
            );
        }
    }

    /// The on-screen keyboard is bound to a shell verb, on the same terms as
    /// the radio pickers above: it is the shell that draws it and decides
    /// whether it is up, so the compositor's part is forwarding a keybinding
    /// rather than owning any state of its own.
    #[test]
    fn the_on_screen_keyboard_is_bound_to_a_shell_verb() {
        let bindings = defaults("foot", Some("wmenu-run"), "tiling");
        let wanted = parse_chord("Mod4+Shift+k").expect("the test's own chord parses");
        let found = bindings.iter().find(|binding| {
            binding.mode.is_empty()
                && binding.keysym == wanted.keysym
                && binding.modifiers == wanted.modifiers
        });
        assert_eq!(
            found.map(|binding| binding.action.clone()),
            Some(Action::Shell("osk".to_owned())),
            "Mod4+Shift+k should toggle the on-screen keyboard"
        );
    }

    /// `Mod4+d` opens the built-in launcher unless an external menu is named.
    ///
    /// The launcher is the shell's to draw and the compositor's to feed, so
    /// the default is a shell verb on the same terms as the pickers above —
    /// and a `menu` in the config file is how somebody keeps the external one,
    /// which is why the chord goes back to `exec` when one is named.
    #[test]
    fn the_menu_chord_is_the_launcher_unless_a_menu_is_named() {
        let chord = parse_chord("Mod4+d").expect("the test's own chord parses");
        let action = |bindings: &[Binding]| {
            bindings
                .iter()
                .find(|binding| {
                    binding.mode.is_empty()
                        && binding.keysym == chord.keysym
                        && binding.modifiers == chord.modifiers
                })
                .map(|binding| binding.action.clone())
        };

        assert_eq!(
            action(&defaults("foot", None, "tiling")),
            Some(Action::Shell("launcher".to_owned())),
            "no menu named: the built-in launcher"
        );
        assert_eq!(
            action(&defaults("foot", Some("wmenu-run -i"), "tiling")),
            Some(Action::Exec("wmenu-run -i".to_owned())),
            "a menu named: the external one"
        );
    }

    /// Every chord the shell is shown can be typed back into a config file.
    ///
    /// That is the property worth having: the listing exists for someone who
    /// wants to change one of them, and a chord written in a notation the
    /// parser does not accept has to be translated by hand before it is any
    /// use. Round-tripping the whole default keymap is the cheapest way to
    /// know it holds for every kind of binding at once.
    #[test]
    fn every_chord_reads_back_the_way_it_was_written() {
        for layout in ["tiling", "scrolling", "solar", "matrix", "canvas"] {
            for binding in defaults("foot", Some("wmenu-run"), layout) {
                let text = binding.chord();
                let parsed = parse_chord(&text)
                    .unwrap_or_else(|| panic!("{layout}: {text:?} does not parse"));
                assert_eq!(parsed.keysym, binding.keysym, "{layout}: {text}");
                assert_eq!(parsed.modifiers, binding.modifiers, "{layout}: {text}");
                assert_eq!(parsed.button, binding.button, "{layout}: {text}");
            }
        }
    }

    /// And what it does is spelled the way a config file spells that too, so
    /// the two halves of a listing are both things someone can act on.
    #[test]
    fn an_action_reads_back_the_way_it_was_written() {
        let cases = [
            ("Mod4+Return=exec foot", "exec foot"),
            ("Mod4+Shift+q=close", "close"),
            ("Mod4+h=focus left", "focus left"),
            ("Mod4+r=mode resize", "mode resize"),
            ("resize/Escape=mode default", "mode default"),
            ("Mod4+o=shell layout.overview", "shell layout.overview"),
            ("Mod4+Shift+x=lock", "lock"),
        ];
        for (spec, wanted) in cases {
            let binding = parse(spec).expect("the test's own spec parses");
            assert_eq!(binding.action_text(), wanted, "{spec}");
            // And back through the parser, which is the whole claim.
            assert_eq!(
                parse_action(&binding.action_text()),
                binding.action,
                "{spec}"
            );
        }
    }

    #[test]
    fn scrolling_sends_focus_to_the_shell_differently() {
        let tiling = defaults("foot", Some("wmenu-run"), "tiling");
        let scrolling = defaults("foot", Some("wmenu-run"), "scrolling");
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
        let solar = defaults("foot", Some("wmenu-run"), "solar");
        assert_eq!(
            find(&solar),
            Some(Action::Shell("solar.ray left".to_owned()))
        );
    }

    /// And Tab follows the same rule, for the same reason.
    ///
    /// A layout that keeps windows outside the view reports them as not on
    /// screen, and the compositor's cycle walks what is on screen — so in the
    /// strip and on the plane the chord that exists to reach the window you
    /// cannot see could not reach it.
    #[test]
    fn tab_cycles_through_the_windows_the_layout_can_see() {
        let tab = |layout: &str, shift: bool| {
            defaults("foot", Some("wmenu-run"), layout)
                .iter()
                .find(|b| {
                    b.mode.is_empty() && b.keysym == keysyms::KEY_Tab && b.modifiers.shift == shift
                })
                .map(|b| b.action.clone())
        };

        // Every window is drawn, so the compositor knows where they all are.
        for layout in ["tiling", "solar", "matrix"] {
            assert_eq!(
                tab(layout, false),
                Some(Action::Focus("next".to_owned())),
                "{layout}"
            );
            assert_eq!(
                tab(layout, true),
                Some(Action::Focus("prev".to_owned())),
                "{layout}"
            );
        }

        // These two do not, so the shell answers it.
        for layout in ["scrolling", "canvas"] {
            assert_eq!(
                tab(layout, false),
                Some(Action::Shell("layout.focus next".to_owned())),
                "{layout}"
            );
            assert_eq!(
                tab(layout, true),
                Some(Action::Shell("layout.focus prev".to_owned())),
                "{layout}"
            );
        }
    }
}

#[cfg(test)]
mod exit_tests {
    use super::*;

    fn exits(bindings: &[Binding]) -> bool {
        bindings
            .iter()
            .any(|b| b.action == Action::Exit && b.mode.is_empty())
    }

    /// An exit a key actually reaches, which is what `guarantee_an_exit` is
    /// for: asked of the matcher rather than of the list, so the test agrees
    /// with what a keypress does.
    fn reachable_exit(bindings: &[Binding]) -> bool {
        bindings.iter().any(|b| {
            b.action == Action::Exit
                && b.mode.is_empty()
                && b.button.is_none()
                && b.wheel.is_none()
                && match_binding(
                    bindings,
                    &ModifiersState {
                        shift: b.modifiers.shift,
                        ctrl: b.modifiers.ctrl,
                        alt: b.modifiers.alt,
                        logo: b.modifiers.logo,
                        ..Default::default()
                    },
                    b.keysym,
                    "",
                ) == Some(&Action::Exit)
        })
    }

    #[test]
    fn the_defaults_already_leave() {
        let mut bindings = defaults("foot", Some("wmenu-run"), "tiling");
        let before = bindings.len();
        guarantee_an_exit(&mut bindings);
        assert_eq!(bindings.len(), before, "nothing was needed");
        assert!(exits(&bindings));
    }

    #[test]
    fn a_config_that_forgot_one_gets_it_back() {
        // `binds` replaces the defaults, so this is a whole config file.
        let mut bindings: Vec<Binding> = ["Mod4+Return=exec foot"]
            .iter()
            .filter_map(|spec| parse(spec))
            .collect();
        guarantee_an_exit(&mut bindings);
        assert!(exits(&bindings), "a session with no way out of it");
    }

    #[test]
    fn a_chord_the_file_claimed_is_not_taken_back() {
        // Adding an exit here would fire whatever the file asked for *and*
        // quit, or shadow it — either way it is not what the file says.
        let mut bindings: Vec<Binding> = ["Mod4+Shift+e=exec firefox"]
            .iter()
            .filter_map(|spec| parse(spec))
            .collect();
        guarantee_an_exit(&mut bindings);
        assert_eq!(bindings.len(), 1);
        assert!(!exits(&bindings));
    }

    #[test]
    fn an_exit_on_some_other_chord_is_enough() {
        let mut bindings: Vec<Binding> = ["Mod4+q=exit"]
            .iter()
            .filter_map(|spec| parse(spec))
            .collect();
        guarantee_an_exit(&mut bindings);
        assert_eq!(bindings.len(), 1, "there is already a way out");
    }

    #[test]
    fn an_exit_that_only_works_in_a_mode_is_not_a_way_out() {
        // A binding mode is a second keymap: nothing in it matches until the
        // mode is entered, and entering it is itself a binding the file may
        // not have made.
        let mut bindings: Vec<Binding> = ["resize/e=exit"]
            .iter()
            .filter_map(|spec| parse(spec))
            .collect();
        guarantee_an_exit(&mut bindings);
        assert!(exits(&bindings), "a moded exit left the session with none");
    }

    #[test]
    fn a_shadowed_exit_is_not_a_way_out() {
        // What `{"binds_override": {"Mod4+q": null}}` produces over a default
        // that put the exit there: the override goes in front, matching is
        // first-wins, and the exit behind it is a binding no key reaches.
        // Counting it left the session with nothing that quits.
        let mut bindings: Vec<Binding> = ["Mod4+q=none", "Mod4+q=exit"]
            .iter()
            .filter_map(|spec| parse(spec))
            .collect();
        guarantee_an_exit(&mut bindings);
        assert!(
            reachable_exit(&bindings),
            "the only exit was behind an unbind"
        );
    }

    #[test]
    fn an_exit_shadowed_on_the_fallback_chord_is_left_alone() {
        // The same shadowing, on Mod4+Shift+E itself. Nothing can be added:
        // the chord is claimed, and binding it anyway would do something the
        // file did not ask for. Ctrl+Alt+Backspace is the way out, and the
        // warning says so.
        let mut bindings: Vec<Binding> = ["Mod4+Shift+e=none", "Mod4+Shift+e=exit"]
            .iter()
            .filter_map(|spec| parse(spec))
            .collect();
        guarantee_an_exit(&mut bindings);
        assert_eq!(bindings.len(), 2, "a chord the file claimed was taken back");
        assert!(!reachable_exit(&bindings));
    }
}
