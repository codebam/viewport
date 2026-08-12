// SPDX-License-Identifier: GPL-3.0-or-later
//
// Which engine draws the desktop.
//
// The shell is a web page, and which engine renders it was for a long time not
// a question: WPE WebKit, embedded, because it is the only engine that hands
// an embedder a DMA-BUF per frame without being asked twice. The cost is that
// `wpewebkit` is packaged by nobody — the flake builds it from source, four
// hours, and no binary cache has it.
//
// So this is a choice now, and the point of naming all of them is that the
// choice is visible at install time rather than being whichever one somebody
// compiled. All of them are implemented:
//
// * `wpe` — the engine in-process, `crate::shell`. Fewest moving parts at run
//   time and the most at build time. Needs `--features wpe`.
//
// * `webkitgtk` — the same WebKit, one version and one port apart, in a
//   process of its own as an ordinary Wayland client. See
//   `crate::shell_client`. nixpkgs ships it prebuilt.
//
// * `chromium` — Blink, out of process, and not linked at all: the browser is
//   started as a child and driven over the DevTools protocol. The engine is
//   whatever `chromium` is on PATH, which makes this the cheapest of the three
//   to build and the only one that can change engine version without a
//   recompile.
//
// Two more are Servo, which is a cargo dependency rather than a package, and
// so is split the same way Blink is split between `cef` and `chromium` — the
// engine linked, and the engine driven:
//
// * `servo` — the `servo` crate, embedded in `viewport-shell-servo`. The
//   engine is compiled from source, once: that crate is outside this
//   workspace, with a lock file of its own, so `cargo test --workspace`, CI
//   and every compositor rebuild leave it alone. Nothing here links it.
//
// * `servoshell` — the same engine as a browser this compositor starts, which
//   is what nixpkgs' `servo` package installs. It compiles no Servo at all:
//   the bridge is a loopback HTTP server in the shell process and a user
//   script `servoshell --userscripts` injects into the page.
//
// * `cef` — the same Blink as `chromium`, embedded as a library rather than
//   driven as a browser. The default. `crates/viewport-shell-cef` is outside
//   this workspace for the same reason `viewport-shell-servo` is: it does not
//   build without `CEF_PATH`, and `cargo test --workspace` has to run on a
//   machine that has never heard of it.
//
// A name is refused only when this binary cannot honour it — `wpe` in a build
// that did not compile the engine in. Everything else is a program that either
// exists beside this one or does not, which `crate::shell_client` reports when
// it goes looking, because "not installed" is a different answer from "no such
// backend" and only one of them is worth falling back over.

use std::fmt;

/// The engine drawing the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellBackend {
    /// WPE WebKit, in this process.
    Wpe,
    /// WebKitGTK, in a process of its own, as a Wayland client.
    WebKitGtk,
    /// Chromium, as a child process driven over the DevTools protocol.
    Chromium,
    /// Servo, embedded in a process of its own by `viewport-shell-servo`.
    Servo,
    /// Servo as a browser — nixpkgs' `servoshell` — started as a child process
    /// and spoken to through a user script. Links no engine.
    ServoShell,
    /// Chromium through CEF, embedded in a process of its own rather than
    /// driven over a socket as `Chromium` is. The default.
    Cef,
}

impl ShellBackend {
    /// The name used by `--shell-backend`, the config file and the log.
    pub fn name(self) -> &'static str {
        match self {
            Self::Wpe => "wpe",
            Self::WebKitGtk => "webkitgtk",
            Self::Chromium => "chromium",
            Self::Servo => "servo",
            Self::ServoShell => "servoshell",
            Self::Cef => "cef",
        }
    }

    /// Every name that can be asked for, whether or not it works here.
    pub const NAMES: &'static [&'static str] =
        &["wpe", "webkitgtk", "chromium", "servo", "servoshell", "cef"];

    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "wpe" => Ok(Self::Wpe),
            "webkitgtk" | "gtk" => Ok(Self::WebKitGtk),
            "chromium" | "blink" => Ok(Self::Chromium),
            "servo" => Ok(Self::Servo),
            "servoshell" => Ok(Self::ServoShell),
            "cef" => Ok(Self::Cef),
            other => Err(format!(
                "no shell backend called '{other}'; it is one of {}",
                Self::NAMES.join(", ")
            )),
        }
    }

    /// What this binary uses when nothing asked for anything.
    ///
    /// The in-process engine where it was compiled in, so a build that has
    /// paid for WebKit behaves as it did before this choice existed. The
    /// out-of-process one otherwise — which is what makes a default `cargo
    /// build` produce a compositor with a desktop, where before it produced
    /// one that came up grey.
    pub fn default_for_build() -> Self {
        if cfg!(feature = "wpe") {
            Self::Wpe
        } else {
            Self::WebKitGtk
        }
    }

    /// Whether this build can actually run it, and why not where it cannot.
    pub fn available(self) -> Result<(), String> {
        match self {
            Self::Wpe if !cfg!(feature = "wpe") => {
                Err("the wpe backend is not in this binary. Rebuild with \
                 `cargo build --release -p viewport --features wpe`, or use \
                 --shell-backend=webkitgtk, which needs no engine compiled in"
                    .to_owned())
            }
            // Everything else is a program beside this one. Whether it is
            // installed is not a question this binary can answer at compile
            // time — `viewport-shell-servo` in particular is built by hand,
            // because building it builds Servo — and `crate::shell_client`
            // says so by name when it cannot find one. Refusing here instead
            // would fall back to another engine on a machine where the asked
            // for one is present, which is the wrong answer twice over.
            Self::Wpe
            | Self::WebKitGtk
            | Self::Chromium
            | Self::Servo
            | Self::ServoShell
            | Self::Cef => Ok(()),
        }
    }

    /// Whether anything drawn *behind* the shell can be seen through it.
    ///
    /// The shell is the bottom layer of the desktop, so a wallpaper terminal
    /// under it is visible only if the engine will composite the page over
    /// nothing. Two will:
    ///
    ///   * **wpe** paints into a buffer this compositor owns and clears to
    ///     transparent, which is what `data/shell/shell.css` has always said.
    ///   * **webkitgtk** takes a transparent `WebView` background and a
    ///     transparent GTK window, which `viewport-shell-gtk` sets when it is
    ///     told there is something behind.
    ///
    /// Chromium will not, in either of the two ways this ships it. CEF's
    /// `background_color = 0` is honoured by the *document* and the Views
    /// window still paints Chromium's own #1f1f1f over it — measured, with
    /// every one of `BrowserSettings::background_color`, `View`'s and
    /// `Window`'s set to transparent, and the composited output is that colour
    /// across the whole screen. Windowed Chromium has no translucent-surface
    /// path on Wayland; the transparent-painting one is windowless rendering,
    /// which is a different backend to the one here.
    ///
    /// So this is a real capability and not a preference, and the answer
    /// decides whether the terminal is started at all — an invisible terminal
    /// under an opaque desktop is a process nobody can see burning a core.
    ///
    /// **This list is a measurement, not an oversight.** Reading the gate
    /// without the history suggests the fix is to add `Cef` here, and the
    /// transparency calls that would need are already written and already
    /// insufficient: `viewport-shell-cef` sets all three of them from
    /// `VIEWPORT_SHELL_TRANSPARENT`, which `shell_client` exports to every
    /// out-of-process shell whenever a background command is configured. They
    /// are kept because they cost nothing and are what a translucent Wayland
    /// surface would need on the day Chromium grows one. Adding the variant
    /// restores the original bug rather than fixing anything.
    ///
    /// `Chromium` is further out of reach than `Cef`: that window belongs to a
    /// browser this compositor started rather than one it links, and no
    /// DevTools call makes a foreign Wayland surface translucent —
    /// `Emulation.setDefaultBackgroundColorOverride` changes what the document
    /// composites over, not the surface under it.
    ///
    /// The route that does work is CEF's windowless rendering, where
    /// `OnAcceleratedPaint` hands over DMA-BUF planes, a modifier and a format
    /// — see the header of `crates/viewport-shell-cef/src/main.rs`. That is a
    /// different rendering backend and worth having for its own sake; the
    /// wallpaper would follow from it rather than motivate it.
    ///
    /// Neither Servo backend is here either, and for a plainer reason than
    /// Chromium's: `WindowRenderingContext` asks surfman for a window surface
    /// with the display's own configuration and no alpha, and `servoshell`
    /// offers no flag that changes it. The page composites over Servo's white,
    /// so a terminal under it would be a process nobody can see.
    pub fn shows_what_is_behind(self) -> bool {
        matches!(self, Self::Wpe | Self::WebKitGtk)
    }

    /// Whether the shell runs in a process of its own.
    pub fn is_out_of_process(self) -> bool {
        matches!(
            self,
            Self::WebKitGtk | Self::Chromium | Self::Servo | Self::ServoShell | Self::Cef
        )
    }

    /// The program the compositor starts for an out-of-process backend.
    ///
    /// One binary per engine rather than one that switches: they share the
    /// socket half (`viewport-shell-bridge`) and nothing else, and a build that
    /// wants WebKitGTK should not have to link Chromium's launcher to get it.
    pub fn shell_program(self) -> Option<&'static str> {
        match self {
            Self::WebKitGtk => Some("viewport-shell-gtk"),
            Self::Chromium => Some("viewport-shell-chromium"),
            Self::Cef => Some("viewport-shell-cef"),
            Self::Servo => Some("viewport-shell-servo"),
            Self::ServoShell => Some("viewport-shell-servoshell"),
            Self::Wpe => None,
        }
    }
}

impl fmt::Display for ShellBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What to run, from the flag, the environment and the config file in that
/// order.
///
/// Nothing here refuses: a name that cannot be honoured is reported by the
/// caller, once, beside what it fell back to. A compositor that will not start
/// because of one line in a config file is a session that cannot be logged
/// into to fix it.
pub fn choose(flag: Option<&str>, configured: Option<&str>) -> ShellBackend {
    let asked = flag
        .map(str::to_owned)
        .or_else(|| std::env::var("VIEWPORT_SHELL_BACKEND").ok())
        .or_else(|| configured.map(str::to_owned));

    let Some(asked) = asked else {
        return ShellBackend::default_for_build();
    };

    match ShellBackend::parse(&asked) {
        Ok(backend) => match backend.available() {
            Ok(()) => backend,
            Err(why) => {
                let fallback = ShellBackend::default_for_build();
                tracing::error!("{why}");
                tracing::warn!("falling back to the {fallback} shell backend");
                fallback
            }
        },
        Err(why) => {
            let fallback = ShellBackend::default_for_build();
            tracing::error!("{why}");
            tracing::warn!("falling back to the {fallback} shell backend");
            fallback
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_parses_to_itself() {
        for name in ShellBackend::NAMES {
            let backend = ShellBackend::parse(name).expect("a listed name must parse");
            assert_eq!(backend.name(), *name);
        }
    }

    #[test]
    fn an_unknown_name_lists_the_ones_that_exist() {
        let error = ShellBackend::parse("firefox").unwrap_err();
        for name in ShellBackend::NAMES {
            assert!(error.contains(name), "{error} does not mention {name}");
        }
    }

    /// Only what this binary cannot do may be refused.
    ///
    /// `choose` falls back on that answer, so a backend refused here is a
    /// backend nobody can select — and every one of these is a program beside
    /// the compositor, whose absence is a message from `shell_client` rather
    /// than grounds for quietly drawing the desktop with another engine.
    #[test]
    fn only_an_engine_this_build_lacks_is_refused() {
        assert!(ShellBackend::WebKitGtk.available().is_ok());
        assert!(ShellBackend::Chromium.available().is_ok());
        assert!(ShellBackend::Servo.available().is_ok());
        assert!(ShellBackend::ServoShell.available().is_ok());
        assert!(ShellBackend::Cef.available().is_ok());
        assert_eq!(
            ShellBackend::Wpe.available().is_err(),
            !cfg!(feature = "wpe")
        );
    }

    /// Every backend that runs in its own process must name the program that
    /// process is, and no other backend may — a compositor that tried to start
    /// a shell for the in-process engine would start two of them.
    #[test]
    fn out_of_process_backends_name_a_program() {
        for name in ShellBackend::NAMES {
            let backend = ShellBackend::parse(name).expect("a listed name parses");
            assert_eq!(
                backend.is_out_of_process(),
                backend.shell_program().is_some(),
                "{backend} disagrees with itself about whether it has a program"
            );
        }
    }

    #[test]
    fn a_backend_that_cannot_run_falls_back_to_one_that_can() {
        let chosen = choose(Some("lynx"), None);
        assert_eq!(chosen, ShellBackend::default_for_build());
        assert!(chosen.available().is_ok());
    }
}
