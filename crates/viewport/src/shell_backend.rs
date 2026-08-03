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
// compiled. Three are implemented:
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
// And two are named, refused, and documented, because "not implemented" and
// "not compiled in" and "no such thing" are three different answers and a
// config file that names one deserves to be told which:
//
// * `servo` — the original plan for the rewrite, spiked as far as the buffer
//   handoff. `crates/viewport-web/src/dmabuf.rs` is that spike and it works;
//   what is missing is the `RenderingContext` and `WebEngine` implementation
//   over it. nixpkgs has a `servo` package, but it builds servoshell — an
//   embedding is a cargo dependency on the `servo` crate either way, so this
//   one buys a supported engine rather than a shorter build.
//
// * `cef` — the same Blink as `chromium`, but embedded as a library rather
//   than driven as a browser, whose offscreen rendering hands over dmabuf
//   planes in very nearly the shape `viewport_web::Frame` already has. That is
//   the version worth having in-process, and it is the one piece of real work
//   left here. What it costs, measured rather than guessed:
//
//     - crates.io `cef` is 149.3.0+149.0.6 and nixpkgs' `cef-binary` is
//       149.0.5, so one of the two has to move.
//     - `cef-dll-sys`'s build script builds `libcef_dll_wrapper` from the
//       distribution with cmake and ninja, and *downloads* a CEF archive
//       unless `CEF_PATH` holds one with an `archive.json` it recognises —
//       which a nix sandbox forbids, so that file has to be produced.
//     - CEF re-executes the host binary for its zygote, GPU and render
//       processes, so the entry point has to hand off before anything else
//       runs.

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
    /// Servo, in this process. Not implemented.
    Servo,
    /// Chromium through CEF, in this process. Not implemented.
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
            Self::Cef => "cef",
        }
    }

    /// Every name that can be asked for, whether or not it works here.
    pub const NAMES: &'static [&'static str] = &["wpe", "webkitgtk", "chromium", "servo", "cef"];

    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "wpe" => Ok(Self::Wpe),
            "webkitgtk" | "gtk" => Ok(Self::WebKitGtk),
            "chromium" | "blink" => Ok(Self::Chromium),
            "servo" => Ok(Self::Servo),
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
            Self::Wpe | Self::WebKitGtk | Self::Chromium | Self::Cef => Ok(()),
            Self::Servo => Err("the servo backend is not implemented yet: \
                                the buffer handoff is spiked in \
                                crates/viewport-web/src/dmabuf.rs and the engine over it is not \
                                written. Use --shell-backend=webkitgtk"
                .to_owned()),
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
    pub fn shows_what_is_behind(self) -> bool {
        matches!(self, Self::Wpe | Self::WebKitGtk)
    }

    /// Whether the shell runs in a process of its own.
    pub fn is_out_of_process(self) -> bool {
        matches!(self, Self::WebKitGtk | Self::Chromium | Self::Cef)
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
            Self::Wpe | Self::Servo => None,
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

    /// What is not written must not be silently treatable as available:
    /// `choose` falls back on that answer, and a fallback that did not happen
    /// is a session with no desktop.
    #[test]
    fn the_unimplemented_backend_is_refused() {
        assert!(ShellBackend::Servo.available().is_err());
        assert!(ShellBackend::WebKitGtk.available().is_ok());
        assert!(ShellBackend::Chromium.available().is_ok());
        assert!(ShellBackend::Cef.available().is_ok());
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
        let chosen = choose(Some("servo"), None);
        assert_eq!(chosen, ShellBackend::default_for_build());
        assert!(chosen.available().is_ok());
    }
}
