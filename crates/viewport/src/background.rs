// SPDX-License-Identifier: GPL-3.0-or-later
//
// A terminal drawn as the wallpaper.
//
// `background_terminal` starts the configured terminal emulator and draws it
// underneath the shell, across the whole output layout, in place of the
// desktop background. It is the same client any other terminal would be — the
// compositor grows no way to run a program that it did not already have, and
// the emulator owns its pty exactly as it does in a window.
//
// **It never receives input, and that is the feature.** A terminal on the
// desktop that could be typed into would be a shell prompt sitting under every
// window, reachable by any keystroke that failed to land somewhere else: a
// focus bug, a race between a window closing and the next one being focused,
// or a password typed a moment after the window it was meant for went away
// would all be delivered to a command line. So this client is:
//
//   * not a view — it is never registered in `views`, so `view.focus` cannot
//     name it and the shell is never told it exists,
//   * not in the `Space` — so pointer hit-testing walks straight past it,
//   * not the shell — `ShellFocus` moves the keyboard to the shell's surface,
//     and this is a different connection with a different flag,
//
// which leaves no path in the compositor by which a key or a click reaches it.
// What it is for is `btop`, `journalctl -f`, a clock, a log — something to
// look at. Running an interactive shell there is allowed and pointless: it
// will sit at its prompt forever.
//
// The identity trick is the shell's, for the same reason (`shell_client.rs`):
// the compositor makes the socket pair itself and hands one end to the process
// it spawned, so "is this the background" is a property of the connection.
// Recognising it by `app_id` would let any client name itself into the one
// position on the desktop that is drawn under everything and told about
// nothing.

use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::shell::xdg::ToplevelSurface;

use crate::state::{ClientState, ViewportState};

/// How many restarts in a window before it is left down.
///
/// The shell's policy (`crate::shell_client::RESTART_LIMIT`) applied to
/// something far less important: a wallpaper that will not stay up is worth a
/// few attempts and then a line in the log, not a fork bomb behind a desktop
/// that is otherwise working.
const RESTART_LIMIT: u32 = 5;
const RESTART_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a terminal asked to close has before it is killed.
///
/// Long enough for a shell to run its exit traps and for a terminal to tear
/// its window down, short enough that a client which ignores the request is
/// not still painting a frame a second under an opaque wallpaper a minute
/// later.
const CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

pub struct BackgroundTerminal {
    child: Child,
    /// The command line it was started with, so a restart runs the same one.
    command: String,
    toplevel: Option<ToplevelSurface>,
    /// The size it was last configured to, so the layout changing is the only
    /// thing that sends another one.
    configured: Option<(u32, u32)>,
    restarts: u32,
    restart_window: Option<std::time::Instant>,
    /// When it was asked to close, because a wallpaper program took the
    /// position. `None` while it is running for its own sake.
    closing: Option<std::time::Instant>,
}

impl BackgroundTerminal {
    pub fn surface(&self) -> Option<&WlSurface> {
        self.toplevel.as_ref().map(|toplevel| toplevel.wl_surface())
    }
}

impl ViewportState {
    /// Start the background terminal, if one was asked for.
    ///
    /// Called from both backends once the outputs exist, beside
    /// `start_shell_process`, and does nothing when the feature is off — which
    /// is what keeps the call sites free of any knowledge of it.
    pub fn start_background_process(&mut self) {
        if self.background_terminal.is_some() {
            return;
        }
        let Some(command) = self.background_command.clone() else {
            return;
        };
        // Something is already drawing the wallpaper. Starting a terminal
        // under an opaque layer surface is starting one nobody will ever see.
        if self.wallpaper_layer_present() {
            tracing::info!(
                "background: {command:?} is not being started — a wallpaper program has \
                 the background layer"
            );
            return;
        }
        if let Err(e) = self.start_background_terminal(&command) {
            // Not fatal, and deliberately so: the desktop is the shell, and a
            // wallpaper that would not start is not a reason to have no
            // session to fix it from.
            tracing::error!("background: could not start {command:?}: {e:#}");
        }
    }

    /// Spawn it and give it a connection of its own.
    ///
    /// The client is inserted into the display before the process exists, for
    /// the reason `start_client_shell` gives: otherwise there is a window in
    /// which it connects and is taken for an ordinary client, which would put
    /// a terminal in the middle of the desktop as a window.
    pub fn start_background_terminal(&mut self, command: &str) -> Result<()> {
        let (ours, theirs) = UnixStream::pair().context("making a socket for the background")?;

        self.display_handle
            .insert_client(
                ours,
                Arc::new(ClientState {
                    background: true,
                    ..Default::default()
                }),
            )
            .map_err(|e| anyhow!("inserting the background's connection: {e}"))?;

        // Cleared between fork and exec, as for the shell: `UnixStream` is
        // close-on-exec and this is the one fd that has to survive.
        let raw = theirs.as_raw_fd();
        let mut child = Command::new("/bin/sh");
        child
            .arg("-c")
            .arg(command)
            .env("WAYLAND_SOCKET", raw.to_string())
            // Its children connect the ordinary way — a program run inside it
            // that opens a window is a window, not another wallpaper.
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("GDK_BACKEND", "wayland");

        // The control socket is not passed on.
        //
        // Every other process in the session can find it anyway — the path is
        // derived from `WAYLAND_DISPLAY` — so this buys nothing against a
        // program that goes looking. It is here because the shell is handed
        // `VIEWPORT_IPC_SOCKET` as a statement that driving the desktop is its
        // job, and the wallpaper is not being told the same thing.
        child.env_remove("VIEWPORT_IPC_SOCKET");

        // SAFETY: `fcntl` is async-signal-safe, which is the whole requirement
        // on a `pre_exec` closure. Nothing here allocates or takes a lock.
        unsafe {
            use std::os::unix::process::CommandExt as _;
            child.pre_exec(move || {
                let fd = std::os::fd::BorrowedFd::borrow_raw(raw);
                smithay::reexports::rustix::io::fcntl_setfd(
                    fd,
                    smithay::reexports::rustix::io::FdFlags::empty(),
                )
                .map_err(std::io::Error::from)
            });
        }

        let child = child
            .spawn()
            .with_context(|| format!("starting {command:?}"))?;
        // Ours is now the child's. Holding it open would mean the compositor
        // never sees the connection close when it dies.
        drop(theirs);

        tracing::info!("background: started {command:?} as pid {}", child.id());

        self.background_terminal = Some(BackgroundTerminal {
            child,
            command: command.to_owned(),
            toplevel: None,
            configured: None,
            restarts: 0,
            restart_window: None,
            closing: None,
        });
        Ok(())
    }

    /// Whether a surface belongs to the connection the compositor made for the
    /// background terminal.
    ///
    /// A property of the connection, not of anything the client said about
    /// itself. See the module comment.
    pub fn is_background_client(&self, surface: &WlSurface) -> bool {
        use smithay::reexports::wayland_server::Resource as _;
        surface
            .client()
            .and_then(|client| client.get_data::<ClientState>().map(|data| data.background))
            .unwrap_or(false)
    }

    /// Take its toplevel instead of letting it become a window.
    ///
    /// A window would be tiled, announced to the shell, listed in the taskbar,
    /// offered as a screen-share source and — the part that matters — made
    /// focusable. The wallpaper is none of those.
    pub fn adopt_background_toplevel(&mut self, toplevel: ToplevelSurface) {
        // A terminal that made a second toplevel — a dialog, an "are you sure
        // you want to close" — gets nothing: there is one wallpaper, and the
        // second surface has nowhere to be. Dropping it here means the client
        // waits for a configure that never comes, rather than being drawn over
        // the first one.
        if self
            .background_terminal
            .as_ref()
            .is_some_and(|background| background.toplevel.is_some())
        {
            tracing::warn!("background: it made a second toplevel, which is ignored");
            return;
        }
        tracing::info!("background: its toplevel arrived");
        if let Some(background) = self.background_terminal.as_mut() {
            background.toplevel = Some(toplevel);
            background.configured = None;
        }
        self.configure_background();
        self.announce_background_outputs();
    }

    /// Tell it which screens it is on.
    ///
    /// `wl_surface.enter`, which nothing else sends it: it is not in the
    /// `Space` and not in a layer map. Without it a client has no output to
    /// take a refresh rate from — see `announce_shell_outputs`, where the same
    /// omission had the shell painting at 60Hz on a 240Hz panel.
    pub fn announce_background_outputs(&mut self) {
        let Some(surface) = self.background_surface().cloned() else {
            return;
        };
        for output in self.space.outputs() {
            output.enter(&surface);
        }
    }

    /// Size it to the whole layout.
    ///
    /// One terminal across every monitor, like the shell — the wallpaper is
    /// one thing, and two screens make it wider rather than making a second
    /// one. A terminal is a grid of cells, so this is also what decides how
    /// many columns it has.
    pub fn configure_background(&mut self) {
        let size = self.layout_size();
        if size.0 == 0 || size.1 == 0 {
            return;
        }
        let Some(background) = self.background_terminal.as_mut() else {
            return;
        };
        if background.configured == Some(size) {
            return;
        }
        let Some(toplevel) = background.toplevel.clone() else {
            return;
        };
        // Not gated on the initial configure having gone out, which is the
        // trap this fell into once already.
        //
        // A client that uses xdg-decoration has already been sent one by the
        // time it commits: `answer_decoration` replies to `new_decoration`
        // with a configure, and every terminal worth running here asks for
        // server-side decorations. So "has it been configured yet" is true
        // before this has said anything about size, and waiting for it meant
        // foot came up at whatever size it chose for itself — a small window
        // in the corner of the wallpaper — and stayed there.
        //
        // `send_configure` is legal either way; it is `send_pending_configure`
        // that is not, before the initial one.
        background.configured = Some(size);
        tracing::info!("background: configuring it to {}x{}", size.0, size.1);
        toplevel.with_pending_state(|state| {
            state.size = Some((size.0 as i32, size.1 as i32).into());
        });
        toplevel.send_configure();
    }

    /// Its surface, for rendering and frame callbacks.
    pub fn background_surface(&self) -> Option<&WlSurface> {
        self.background_terminal
            .as_ref()
            .and_then(|background| background.surface())
    }

    /// Handle a commit from it, and say whether it was one.
    ///
    /// Returns true to stop the ordinary commit path: it is not a window, not
    /// a layer surface and not a lock screen, and every one of those paths
    /// would either ignore it or, worse, half-adopt it.
    pub fn background_commit(&mut self, surface: &WlSurface) -> bool {
        let is_background = self
            .background_terminal
            .as_ref()
            .and_then(|background| background.surface())
            .is_some_and(|background| background == surface);
        if !is_background {
            return false;
        }

        // The size, if it has not been told it yet. Until it is, the client
        // has heard nothing about how big the desktop is and a terminal with
        // no size of its own picks one — which is an 80x24 window in the
        // corner of the wallpaper. `configure_background` sends at most one
        // per layout size, so this costs a comparison per commit.
        //
        // Here rather than at adoption because at adoption there may be no
        // outputs yet: on the DRM backend the process is started before the
        // first monitor is up, and a zero-sized layout configures nothing.
        self.configure_background();

        // It painted, so the screen has changed. The same two clocks every
        // other client's commit arms: it is invited to draw by frame callbacks
        // and is as entitled to a fifo barrier as anything else that paints.
        self.needs_render = true;
        self.arm_frame_clock();
        self.arm_barrier_tick();
        true
    }

    /// Forget its toplevel when it goes.
    ///
    /// The process may still be alive — a client can destroy a toplevel and
    /// make another — so this drops the surface and not the terminal.
    pub fn background_toplevel_destroyed(&mut self, surface: &WlSurface) -> bool {
        let Some(background) = self.background_terminal.as_mut() else {
            return false;
        };
        if background.surface() != Some(surface) {
            return false;
        }
        tracing::info!("background: its toplevel went away");
        background.toplevel = None;
        background.configured = None;
        self.needs_render = true;
        true
    }

    /// Stand down: something else is drawing the wallpaper now.
    ///
    /// swaybg, hyprpaper, mpvpaper, azote — a wallpaper program is a
    /// layer-shell client on the background layer, which is drawn over
    /// everything this module puts on the screen. The terminal underneath it
    /// is then invisible and still painting, which is a program nobody can see
    /// spending a core's worth of a laptop's battery on frames that are thrown
    /// away.
    ///
    /// Asked to close rather than killed, so the shell inside it and whatever
    /// that shell is running get to exit the way they would if the window had
    /// been closed. A client that ignores `xdg_toplevel.close` is killed once
    /// the grace period is up — that is what `closing` is for.
    pub fn background_yield_to_wallpaper(&mut self) {
        // Nothing running, or it has already been told.
        let Some(background) = self.background_terminal.as_mut() else {
            return;
        };
        if background.closing.is_some() {
            return;
        }
        tracing::info!(
            "background: a wallpaper program took the background layer, so {:?} is closing",
            background.command
        );
        background.closing = Some(std::time::Instant::now());
        if let Some(toplevel) = background.toplevel.as_ref() {
            toplevel.send_close();
        }
        // Never mapped, so there is nothing to ask politely: a process that
        // has not put a surface up has nothing to save.
        else {
            let _ = background.child.kill();
        }
    }

    /// Take the wallpaper back, if the program that took it has gone.
    ///
    /// The terminal is started again from scratch rather than resumed: it was
    /// closed, and a closed terminal has no session left to return to.
    pub fn background_reclaim_wallpaper(&mut self) {
        if self.wallpaper_layer_present() {
            return;
        }
        // Still going down. `check_background_terminal` starts the next one
        // when it has actually gone, and starting one now would leave two.
        if self
            .background_terminal
            .as_ref()
            .is_some_and(|background| background.closing.is_some())
        {
            return;
        }
        if self.background_terminal.is_none() && self.background_command.is_some() {
            tracing::info!("background: the wallpaper is free again");
            self.start_background_process();
        }
    }

    /// Whether anything is on the background layer of any output.
    ///
    /// The background layer specifically, and not `Bottom`: a bar or a dock
    /// that sits under the windows is not claiming the wallpaper, and killing
    /// the terminal for one would be a surprise. What lives on `Background` is
    /// wallpaper programs, and that is the whole of it.
    pub fn wallpaper_layer_present(&self) -> bool {
        use smithay::wayland::shell::wlr_layer::Layer;
        self.space.outputs().any(|output| {
            smithay::desktop::layer_map_for_output(output)
                .layers()
                .any(|layer| layer.layer() == Layer::Background)
        })
    }

    /// Notice it dying, and start it again.
    ///
    /// Polled from the slow tick beside `check_client_shell`, and for the same
    /// reason: there is no signal handling here, and a wallpaper that has been
    /// gone for at most a second is not something anybody can see.
    pub fn check_background_terminal(&mut self) {
        let Some(background) = self.background_terminal.as_mut() else {
            return;
        };
        let status = match background.child.try_wait() {
            Ok(Some(status)) => status,
            Ok(None) | Err(_) => {
                // Still running. If it was asked to close and has not, it is
                // ignoring the request — every terminal worth running honours
                // it, and the ones that do not are not entitled to keep
                // painting under a wallpaper for the rest of the session.
                if let Some(asked) = background.closing {
                    if asked.elapsed() > CLOSE_GRACE {
                        tracing::warn!(
                            "background: {:?} ignored the close, so it is being killed",
                            background.command
                        );
                        let _ = background.child.kill();
                    }
                }
                return;
            }
        };

        // Closed on purpose, so this is not a crash and there is nothing to
        // restart: the wallpaper belongs to something else now, and
        // `background_reclaim_wallpaper` starts a new one if it gives it back.
        if background.closing.is_some() {
            tracing::info!(
                "background: {:?} closed for the wallpaper program",
                background.command
            );
            self.background_terminal = None;
            self.needs_render = true;
            // And if the wallpaper program has already gone in the meantime —
            // a swaybg that was killed a moment after it started, which is
            // what changing wallpaper with a restart looks like — the position
            // is free and nothing else would notice.
            self.background_reclaim_wallpaper();
            return;
        }

        let command = background.command.clone();
        let now = std::time::Instant::now();
        let fresh = background
            .restart_window
            .is_none_or(|started| now.duration_since(started) > RESTART_WINDOW);
        if fresh {
            background.restart_window = Some(now);
            background.restarts = 0;
        }
        background.restarts += 1;
        let attempt = background.restarts;

        self.background_terminal = None;
        self.needs_render = true;

        if attempt > RESTART_LIMIT {
            tracing::error!(
                "background: {command:?} exited with {status} — {attempt} times in under {}s, so \
                 it will not be started again",
                RESTART_WINDOW.as_secs()
            );
            return;
        }

        tracing::warn!(
            "background: {command:?} exited with {status}; restarting it \
             ({attempt}/{RESTART_LIMIT})"
        );
        if let Err(e) = self.start_background_terminal(&command) {
            tracing::error!("background: could not restart it: {e:#}");
            return;
        }
        // The budget belongs to the run of crashes, not to the process.
        if let Some(background) = self.background_terminal.as_mut() {
            background.restarts = attempt;
            background.restart_window = Some(now);
        }
    }
}

/// What to run as the wallpaper, from what the config file said.
///
/// `true` means "the terminal this desktop already opens with Mod4+Return",
/// which is the answer that needs no second setting; a string is a command
/// line of its own, for the common case of wanting something specific in it
/// (`"btop"`, `"journalctl -f"`) rather than a login shell nobody can type at.
///
/// Resolved here rather than at the call site so the precedence is in one
/// place and testable without a compositor.
pub fn resolve(
    asked: Option<&crate::config::BackgroundTerminal>,
    terminal: &str,
) -> Option<String> {
    match asked? {
        crate::config::BackgroundTerminal::Enabled(false) => None,
        crate::config::BackgroundTerminal::Enabled(true) => Some(terminal.to_owned()),
        crate::config::BackgroundTerminal::Command(command) if command.trim().is_empty() => None,
        crate::config::BackgroundTerminal::Command(command) => Some(command.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackgroundTerminal as Asked;

    #[test]
    fn absent_and_false_are_both_off() {
        assert_eq!(resolve(None, "foot"), None);
        assert_eq!(resolve(Some(&Asked::Enabled(false)), "foot"), None);
    }

    #[test]
    fn true_runs_the_configured_terminal() {
        assert_eq!(
            resolve(Some(&Asked::Enabled(true)), "alacritty"),
            Some("alacritty".to_owned())
        );
    }

    #[test]
    fn a_string_is_the_command_line() {
        assert_eq!(
            resolve(Some(&Asked::Command("foot -e btop".to_owned())), "foot"),
            Some("foot -e btop".to_owned())
        );
    }

    /// An empty string is a config file that says nothing, not a command.
    /// `/bin/sh -c ""` exits 0 immediately, which would spend the restart
    /// budget five times over and log a crash loop for a typo.
    #[test]
    fn an_empty_command_is_off() {
        assert_eq!(
            resolve(Some(&Asked::Command("   ".to_owned())), "foot"),
            None
        );
    }
}
