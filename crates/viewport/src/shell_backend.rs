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
// So this is a choice now, and the point of naming all four is that the
// choice is visible at install time rather than being whichever one somebody
// compiled. Two are implemented:
//
// * `wpe` — the engine in-process, `crate::shell`. Fewest moving parts at run
//   time and the most at build time. Needs `--features wpe`.
//
// * `webkitgtk` — the same WebKit, one version and one port apart, in a
//   process of its own as an ordinary Wayland client. See
//   `crate::shell_client`. nixpkgs ships it prebuilt.
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
// * `cef` — Chromium through CEF's offscreen rendering, whose
//   `OnAcceleratedPaint` hands over dmabuf planes on Linux in very nearly the
//   shape `viewport_web::Frame` already has. nixpkgs' `cef-binary` is a
//   prebuilt blob, so it is the only option here with no engine build at all;
//   the cost is a C++ API and a multi-process model to host.

use std::fmt;

/// The engine drawing the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellBackend {
    /// WPE WebKit, in this process.
    Wpe,
    /// WebKitGTK, in a process of its own, as a Wayland client.
    WebKitGtk,
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
            Self::Servo => "servo",
            Self::Cef => "cef",
        }
    }

    /// Every name that can be asked for, whether or not it works here.
    pub const NAMES: &'static [&'static str] = &["wpe", "webkitgtk", "servo", "cef"];

    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "wpe" => Ok(Self::Wpe),
            "webkitgtk" | "gtk" => Ok(Self::WebKitGtk),
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
            Self::Wpe | Self::WebKitGtk => Ok(()),
            Self::Servo => Err("the servo backend is not implemented yet: \
                                the buffer handoff is spiked in \
                                crates/viewport-web/src/dmabuf.rs and the engine over it is not \
                                written. Use --shell-backend=webkitgtk"
                .to_owned()),
            Self::Cef => Err("the cef backend is not implemented yet: \
                              CEF's OnAcceleratedPaint is the right shape for this and nothing \
                              is written against it. Use --shell-backend=webkitgtk"
                .to_owned()),
        }
    }

    /// Whether the shell runs in a process of its own.
    pub fn is_out_of_process(self) -> bool {
        matches!(self, Self::WebKitGtk)
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

    /// The two that are not written must not be silently treatable as
    /// available: `choose` falls back on them, and a fallback that did not
    /// happen is a session with no desktop.
    #[test]
    fn the_unimplemented_backends_are_refused() {
        assert!(ShellBackend::Servo.available().is_err());
        assert!(ShellBackend::Cef.available().is_err());
        assert!(ShellBackend::WebKitGtk.available().is_ok());
    }

    #[test]
    fn a_backend_that_cannot_run_falls_back_to_one_that_can() {
        let chosen = choose(Some("servo"), None);
        assert_eq!(chosen, ShellBackend::default_for_build());
        assert!(chosen.available().is_ok());
    }
}
