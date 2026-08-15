// SPDX-License-Identifier: GPL-3.0-or-later
//
// A terminal drawn as the wallpaper.
//
// `background_terminal` starts a terminal emulator on every monitor and draws
// it underneath the shell, in place of the desktop background. It is the same
// client any other terminal would be — the compositor grows no way to run a
// program that it did not already have, and the emulator owns its pty exactly
// as it does in a window.
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
// One per monitor, unlike the shell, which is one page across the whole
// layout. A terminal is a grid of cells and the cells have to land on a
// screen: stretched across two monitors, half the columns are on the other
// one and a line of output is cut down the middle by the gap between them. So
// each output gets a process of its own, sized to that output, told which one
// it is on through `VIEWPORT_OUTPUT` — which is also what makes
// `background_terminal` useful per screen, one showing logs and the other a
// clock, from a command that reads that variable.
//
// The identity trick is the shell's, for the same reason (`shell_client.rs`):
// the compositor makes the socket pair itself and hands one end to the process
// it spawned, so "is this the background" is a property of the connection.
// Recognising it by `app_id` would let any client name itself into the one
// position on the desktop that is drawn under everything and told about
// nothing. Which of them is which monitor is the same fact — the client id the
// connection was made under, not anything the client says.

use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use smithay::output::Output;
use smithay::reexports::wayland_server::backend::ClientId;
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
    /// The monitor it is the wallpaper of, by name. Outputs come and go and
    /// this is what survives a hotplug: the terminal for `DP-2` is the one to
    /// close when `DP-2` is unplugged.
    output: String,
    /// The connection it was given, which is how its surfaces are recognised.
    ///
    /// The `background` flag on `ClientState` says "this is a wallpaper"; this
    /// says *which* wallpaper, and neither is anything the client can claim.
    client: ClientId,
    child: Child,
    /// The command line it was started with, so a restart runs the same one.
    command: String,
    toplevel: Option<ToplevelSurface>,
    /// The size it was last configured to, so its monitor changing mode is the
    /// only thing that sends another one.
    configured: Option<(i32, i32)>,
    restarts: u32,
    restart_window: Option<std::time::Instant>,
    /// When it was asked to close, because a wallpaper program took the
    /// position. `None` while it is running for its own sake.
    closing: Option<std::time::Instant>,
    /// The monitor its surface has been told it is on, by name.
    ///
    /// `wl_surface.enter` and `leave` are a pair a client is entitled to
    /// count: toolkits keep a list of the outputs a surface is on and take the
    /// scale and the refresh rate from it, and an enter with no matching leave
    /// makes that list grow every time the layout changes. So this is what has
    /// been said, and only the difference is sent.
    entered: Option<String>,
}

impl BackgroundTerminal {
    pub fn surface(&self) -> Option<&WlSurface> {
        self.toplevel.as_ref().map(|toplevel| toplevel.wl_surface())
    }
}

impl ViewportState {
    /// Start a terminal on every monitor that has not got one.
    ///
    /// Called from the backends once the outputs exist, beside
    /// `start_shell_process`, and again whenever the layout changes — a
    /// monitor plugged in half an hour later gets its own, and one that
    /// already has a terminal is left alone. Does nothing when the feature is
    /// off, which is what keeps the call sites free of any knowledge of it.
    pub fn start_background_process(&mut self) {
        let Some(command) = self.background_command.clone() else {
            return;
        };
        // An engine that will not let anything show through it. The terminal
        // would run, paint and be covered by the desktop for the whole
        // session, which looks exactly like the feature not working — so say
        // what is wrong and what to do about it, and start nothing.
        if !self.shell_backend.shows_what_is_behind() {
            // Once, not once per monitor and not once per layout change.
            if !self.background_backend_warned {
                self.background_backend_warned = true;
                // Where that engine came from, because the surprising case is
                // an inherited one: a session exports VIEWPORT_SHELL_BACKEND
                // to everything it starts, so a compositor run from a terminal
                // inside another desktop is handed that desktop's engine and
                // the refusal names a backend nobody typed.
                let from = match std::env::var("VIEWPORT_SHELL_BACKEND") {
                    Ok(value) => format!(" (VIEWPORT_SHELL_BACKEND={value})"),
                    Err(_) => String::new(),
                };
                tracing::error!(
                    "background: {command:?} is not being started — the {} shell{from} paints an \
                     opaque background and nothing behind it can be seen. Start the compositor \
                     with --shell-backend=webkitgtk to use a terminal as the wallpaper",
                    self.shell_backend
                );
            }
            return;
        }

        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        for output in outputs {
            let name = output.name();
            if self
                .background_terminals
                .iter()
                .any(|background| background.output == name)
            {
                continue;
            }
            // Something is already drawing this monitor's wallpaper. Starting
            // a terminal under an opaque layer surface is starting one nobody
            // will ever see.
            if self.wallpaper_layer_on(&output) {
                tracing::info!(
                    "background: {command:?} is not being started on {name} — a wallpaper \
                     program has the background layer there"
                );
                continue;
            }
            if let Err(e) = self.start_background_terminal(&name, &command) {
                // Not fatal, and deliberately so: the desktop is the shell,
                // and a wallpaper that would not start is not a reason to have
                // no session to fix it from.
                tracing::error!("background: could not start {command:?} on {name}: {e:#}");
            }
        }
    }

    /// Drop the terminals whose monitors have gone.
    ///
    /// A wallpaper for a screen that is not there paints frames nobody can
    /// see, and on the DRM backend an unplugged monitor is a routine event
    /// rather than an exceptional one.
    pub fn prune_background_terminals(&mut self) {
        let names: Vec<String> = self.space.outputs().map(|output| output.name()).collect();
        let mut gone = Vec::new();
        self.background_terminals.retain_mut(|background| {
            if names.contains(&background.output) {
                return true;
            }
            gone.push(background.output.clone());
            // Killed rather than asked: there is no monitor to draw the
            // closing frame on, and the process holds a surface for an output
            // that no longer exists.
            let _ = background.child.kill();
            // And reaped, as `check_background_terminal` reaps the ones that
            // exited on their own. Nothing in this compositor handles SIGCHLD,
            // so a `Child` dropped without being waited for is a zombie for the
            // rest of the session — one per monitor unplugged, which on a
            // laptop is one per time it is docked.
            let _ = background.child.wait();
            false
        });
        for output in gone {
            tracing::info!("background: {output} went away, so its terminal did too");
            self.needs_render = true;
        }
    }

    /// Spawn one and give it a connection of its own.
    ///
    /// The client is inserted into the display before the process exists, for
    /// the reason `start_client_shell` gives: otherwise there is a window in
    /// which it connects and is taken for an ordinary client, which would put
    /// a terminal in the middle of the desktop as a window.
    pub fn start_background_terminal(&mut self, output: &str, command: &str) -> Result<()> {
        let (ours, theirs) = UnixStream::pair().context("making a socket for the background")?;

        let client = self
            .display_handle
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
            // Which monitor this one is the wallpaper of, so one command can
            // do something different per screen without needing one setting
            // per screen: `sh -c 'case $VIEWPORT_OUTPUT in DP-1) btop;; *)
            // journalctl -f;; esac'`.
            .env("VIEWPORT_OUTPUT", output)
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

        tracing::info!(
            "background: started {command:?} on {output} as pid {}",
            child.id()
        );

        self.background_terminals.push(BackgroundTerminal {
            output: output.to_owned(),
            client: client.id(),
            child,
            command: command.to_owned(),
            toplevel: None,
            configured: None,
            restarts: 0,
            restart_window: None,
            closing: None,
            entered: None,
        });
        Ok(())
    }

    /// Whether a surface belongs to a connection the compositor made for a
    /// wallpaper terminal.
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

    /// Take a toplevel instead of letting it become a window.
    ///
    /// A window would be tiled, announced to the shell, listed in the taskbar,
    /// offered as a screen-share source and — the part that matters — made
    /// focusable. The wallpaper is none of those.
    pub fn adopt_background_toplevel(&mut self, toplevel: ToplevelSurface) {
        use smithay::reexports::wayland_server::Resource as _;
        let Some(client) = toplevel.wl_surface().client().map(|client| client.id()) else {
            return;
        };
        let Some(background) = self
            .background_terminals
            .iter_mut()
            .find(|background| background.client == client)
        else {
            // A background client with no terminal behind it: the entry was
            // dropped — its monitor went, or it was closed for a wallpaper
            // program — while the process was still coming up.
            tracing::warn!("background: a toplevel arrived for a terminal that is gone");
            return;
        };
        // A terminal that made a second toplevel — a dialog, an "are you sure
        // you want to close" — gets nothing: there is one wallpaper per
        // monitor, and the second surface has nowhere to be. Dropping it here
        // means the client waits for a configure that never comes, rather than
        // being drawn over the first one.
        if background.toplevel.is_some() {
            tracing::warn!(
                "background: {} made a second toplevel, which is ignored",
                background.output
            );
            return;
        }
        let output = background.output.clone();
        tracing::info!("background: its toplevel arrived for {output}");
        background.toplevel = Some(toplevel);
        background.configured = None;
        self.configure_background();
        self.announce_background_outputs();
    }

    /// Tell each one which screen it is on.
    ///
    /// `wl_surface.enter`, which nothing else sends them: they are not in the
    /// `Space` and not in a layer map. Without it a client has no output to
    /// take a refresh rate from — see `announce_shell_outputs`, where the same
    /// omission had the shell painting at 60Hz on a 240Hz panel.
    ///
    /// One output each, unlike the shell, which is on all of them.
    ///
    /// Only what has changed since last time. This runs on every layout change
    /// — a monitor plugged in, a mode change, a window moved between screens —
    /// and an unconditional `enter` is one more than the last time for a
    /// surface that has not moved, with no `leave` to balance it. A client that
    /// counts them then believes its wallpaper is on four monitors.
    pub fn announce_background_outputs(&mut self) {
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        for background in self.background_terminals.iter_mut() {
            let Some(surface) = background.surface().cloned() else {
                continue;
            };
            let name = outputs
                .iter()
                .find(|output| output.name() == background.output)
                .map(|output| output.name());
            if background.entered == name {
                continue;
            }
            // The one it was on, if that is still there: an unplugged monitor
            // took its instances with it, and there is nothing left to say
            // leave to.
            if let Some(before) = background.entered.take() {
                if let Some(output) = outputs.iter().find(|output| output.name() == before) {
                    output.leave(&surface);
                }
            }
            if let Some(output) = outputs.iter().find(|output| Some(output.name()) == name) {
                output.enter(&surface);
            }
            background.entered = name;
        }
    }

    /// Size each one to its own monitor.
    ///
    /// The output's logical size, so a scaled screen gets the terminal it has
    /// room for rather than the one its pixel count suggests. A terminal is a
    /// grid of cells, so this is also what decides how many columns it has.
    pub fn configure_background(&mut self) {
        let sizes: Vec<(String, (i32, i32))> = self
            .space
            .outputs()
            .filter_map(|output| {
                let geometry = self.space.output_geometry(output)?;
                Some((output.name(), (geometry.size.w, geometry.size.h)))
            })
            .collect();
        for background in self.background_terminals.iter_mut() {
            let Some((_, size)) = sizes.iter().find(|(name, _)| *name == background.output) else {
                continue;
            };
            let size = *size;
            if size.0 == 0 || size.1 == 0 {
                continue;
            }
            if background.configured == Some(size) {
                continue;
            }
            let Some(toplevel) = background.toplevel.clone() else {
                continue;
            };
            // Not gated on the initial configure having gone out, which is the
            // trap this fell into once already.
            //
            // A client that uses xdg-decoration has already been sent one by
            // the time it commits: `answer_decoration` replies to
            // `new_decoration` with a configure, and every terminal worth
            // running here asks for server-side decorations. So "has it been
            // configured yet" is true before this has said anything about
            // size, and waiting for it meant foot came up at whatever size it
            // chose for itself — a small window in the corner of the
            // wallpaper — and stayed there.
            //
            // `send_configure` is legal either way; it is
            // `send_pending_configure` that is not, before the initial one.
            background.configured = Some(size);
            tracing::info!(
                "background: configuring {} to {}x{}",
                background.output,
                size.0,
                size.1
            );
            toplevel.with_pending_state(|state| {
                state.size = Some(size.into());
            });
            toplevel.send_configure();
        }
    }

    /// The surface that is this monitor's wallpaper, for rendering and frame
    /// callbacks.
    pub fn background_surface_for(&self, output: &Output) -> Option<&WlSurface> {
        let name = output.name();
        self.background_terminals
            .iter()
            .find(|background| background.output == name)
            .and_then(|background| background.surface())
    }

    /// Every wallpaper surface there is, in no particular order.
    ///
    /// For the walks that are about clients rather than about screens — frame
    /// barriers, and whether anything is waiting on one.
    pub fn background_surfaces(&self) -> Vec<WlSurface> {
        self.background_terminals
            .iter()
            .filter_map(|background| background.surface().cloned())
            .collect()
    }

    /// Handle a commit from one, and say whether it was one.
    ///
    /// Returns true to stop the ordinary commit path: it is not a window, not
    /// a layer surface and not a lock screen, and every one of those paths
    /// would either ignore it or, worse, half-adopt it.
    pub fn background_commit(&mut self, surface: &WlSurface) -> bool {
        let is_background = self
            .background_terminals
            .iter()
            .any(|background| background.surface() == Some(surface));
        if !is_background {
            return false;
        }

        // The size, if it has not been told it yet. Until it is, the client
        // has heard nothing about how big its screen is and a terminal with no
        // size of its own picks one — which is an 80x24 window in the corner of
        // the wallpaper. `configure_background` sends at most one per size, so
        // this costs a comparison per commit.
        //
        // Here rather than at adoption because at adoption there may be no
        // geometry yet: on the DRM backend the process is started before the
        // first monitor is up, and a zero-sized output configures nothing.
        self.configure_background();

        // It painted, so the screen has changed. The same two clocks every
        // other client's commit arms: it is invited to draw by frame callbacks
        // and is as entitled to a fifo barrier as anything else that paints.
        self.needs_render = true;
        self.arm_frame_clock();
        self.arm_barrier_tick();
        true
    }

    /// Forget a toplevel when it goes.
    ///
    /// The process may still be alive — a client can destroy a toplevel and
    /// make another — so this drops the surface and not the terminal.
    pub fn background_toplevel_destroyed(&mut self, surface: &WlSurface) -> bool {
        let Some(background) = self
            .background_terminals
            .iter_mut()
            .find(|background| background.surface() == Some(surface))
        else {
            return false;
        };
        tracing::info!(
            "background: the toplevel on {} went away",
            background.output
        );
        background.toplevel = None;
        background.configured = None;
        // A surface that has gone was never told it left, and the next one is a
        // new surface that has been told nothing at all.
        background.entered = None;
        self.needs_render = true;
        true
    }

    /// Stand down on one monitor: something else is drawing its wallpaper now.
    ///
    /// swaybg, hyprpaper, mpvpaper, azote — a wallpaper program is a
    /// layer-shell client on the background layer, which is drawn over
    /// everything this module puts on the screen. The terminal underneath it
    /// is then invisible and still painting, which is a program nobody can see
    /// spending a core's worth of a laptop's battery on frames that are thrown
    /// away.
    ///
    /// Per monitor, because that is how the programs work: `swaybg -o DP-1`
    /// takes one screen and leaves the other, and the terminal on the screen
    /// it did not take is still worth having.
    ///
    /// Asked to close rather than killed, so the shell inside it and whatever
    /// that shell is running get to exit the way they would if the window had
    /// been closed. A client that ignores `xdg_toplevel.close` is killed once
    /// the grace period is up — that is what `closing` is for.
    pub fn background_yield_to_wallpaper(&mut self, output: &Output) {
        let name = output.name();
        let Some(background) = self
            .background_terminals
            .iter_mut()
            .find(|background| background.output == name)
        else {
            return;
        };
        if background.closing.is_some() {
            return;
        }
        tracing::info!(
            "background: a wallpaper program took the background layer on {name}, so {:?} is \
             closing",
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

    /// Take a monitor's wallpaper back, if the program that took it has gone.
    ///
    /// The terminal is started again from scratch rather than resumed: it was
    /// closed, and a closed terminal has no session left to return to.
    pub fn background_reclaim_wallpaper(&mut self, output: &Output) {
        if self.wallpaper_layer_on(output) {
            return;
        }
        let name = output.name();
        // Still going down. `check_background_terminal` starts the next one
        // when it has actually gone, and starting one now would leave two.
        if self
            .background_terminals
            .iter()
            .any(|background| background.output == name && background.closing.is_some())
        {
            return;
        }
        if self.background_command.is_some()
            && !self
                .background_terminals
                .iter()
                .any(|background| background.output == name)
        {
            tracing::info!("background: the wallpaper on {name} is free again");
            self.start_background_process();
        }
    }

    /// Whether anything is on the background layer of this monitor.
    ///
    /// The background layer specifically, and not `Bottom`: a bar or a dock
    /// that sits under the windows is not claiming the wallpaper, and killing
    /// the terminal for one would be a surprise. What lives on `Background` is
    /// wallpaper programs, and that is the whole of it.
    pub fn wallpaper_layer_on(&self, output: &Output) -> bool {
        use smithay::wayland::shell::wlr_layer::Layer;
        smithay::desktop::layer_map_for_output(output)
            .layers()
            .any(|layer| layer.layer() == Layer::Background)
    }

    /// Give the keyboard to the wallpaper terminal on the active monitor, or
    /// take it back.
    ///
    /// The one way in, and it is a chord someone pressed. Everything else
    /// about this client is arranged so that focus cannot arrive by accident —
    /// it is not a view, so `view.focus` cannot name it; it is not in the
    /// `Space`, so a click on the desktop passes over it; it is not the shell,
    /// so the desktop's own focus path does not reach it. None of that changes
    /// here. What changes is that a deliberate request now exists, because a
    /// terminal nobody can ever type into is a picture of a terminal.
    ///
    /// Toggling, and back to where focus was: the same chord returns the
    /// keyboard to the window that had it, so this is a visit rather than a
    /// mode to escape from. A window that has closed in the meantime leaves
    /// focus nowhere, which is what closing the focused window does anyway.
    pub fn toggle_background_focus(&mut self) {
        use smithay::utils::SERIAL_COUNTER;

        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };

        // Already there: give it back.
        //
        // Asked of the seat rather than trusted from the flag. Focus moves for
        // reasons this never hears about — a window opening and being focused
        // by the shell, a lock screen, a layer surface that asked for the
        // keyboard — and a flag left set through one of those would make the
        // next press "restore" focus from a terminal that had not had it.
        let holds = match (&self.background_focused, keyboard.current_focus()) {
            (Some(name), Some(current)) => self.background_terminals.iter().any(|background| {
                background.output == *name && background.surface() == Some(current.surface())
            }),
            _ => false,
        };
        if holds {
            let restore = self.focus_before_background.take();
            self.background_focused = None;
            let target = restore
                .filter(|id| *id != crate::views::NO_VIEW)
                .and_then(|id| self.views.get(id))
                .and_then(|view| crate::keyboard_focus::KeyboardFocus::for_window(&view.window));
            let restored = target.is_some();
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, target, serial);
            let id = if restored {
                restore.unwrap_or(crate::views::NO_VIEW)
            } else {
                crate::views::NO_VIEW
            };
            self.notify_focus(id);
            tracing::info!("background: the keyboard went back to view {id}");
            return;
        }

        // The wallpaper of the monitor being looked at, which is the one a
        // chord is about. Falling back to the only one there is, because a
        // single-screen desktop has no ambiguity to resolve.
        let output = self
            .active_output
            .as_deref()
            .and_then(|name| self.output_by_name(name))
            .or_else(|| self.space.outputs().next().cloned());
        let Some(output) = output else {
            return;
        };
        let Some(surface) = self.background_surface_for(&output).cloned() else {
            tracing::info!(
                "background: nothing to focus on {} — no wallpaper terminal there",
                output.name()
            );
            return;
        };

        self.focus_before_background = Some(self.focused);
        self.background_focused = Some(output.name());
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, Some(surface.into()), serial);
        // No window is focused now, and the shell has to stop drawing one as
        // if it were — the same thing `ShellFocus` does for the desktop.
        self.notify_focus(crate::views::NO_VIEW);
        tracing::info!(
            "background: the keyboard is on the wallpaper terminal on {}",
            output.name()
        );
    }

    /// Notice one dying, and start it again.
    ///
    /// Polled from the slow tick beside `check_client_shell`, and for the same
    /// reason: there is no signal handling here, and a wallpaper that has been
    /// gone for at most a second is not something anybody can see.
    pub fn check_background_terminal(&mut self) {
        // Which of them have exited, with the state needed to decide what that
        // means. Collected first because restarting one takes `&mut self`.
        let mut dead: Vec<(
            String,
            String,
            std::process::ExitStatus,
            bool,
            u32,
            Option<std::time::Instant>,
        )> = Vec::new();
        for background in self.background_terminals.iter_mut() {
            match background.child.try_wait() {
                Ok(Some(status)) => dead.push((
                    background.output.clone(),
                    background.command.clone(),
                    status,
                    background.closing.is_some(),
                    background.restarts,
                    background.restart_window,
                )),
                Ok(None) | Err(_) => {
                    // Still running. If it was asked to close and has not, it
                    // is ignoring the request — every terminal worth running
                    // honours it, and the ones that do not are not entitled to
                    // keep painting under a wallpaper for the rest of the
                    // session.
                    if let Some(asked) = background.closing {
                        if asked.elapsed() > CLOSE_GRACE {
                            tracing::warn!(
                                "background: {:?} on {} ignored the close, so it is being killed",
                                background.command,
                                background.output
                            );
                            let _ = background.child.kill();
                        }
                    }
                }
            }
        }
        if dead.is_empty() {
            return;
        }

        let names: Vec<String> = dead.iter().map(|(name, ..)| name.clone()).collect();
        self.background_terminals
            .retain(|background| !names.contains(&background.output));
        self.needs_render = true;

        for (name, command, status, closed, restarts, window) in dead {
            // Closed on purpose, so this is not a crash and there is nothing
            // to restart: the wallpaper belongs to something else now, and
            // `background_reclaim_wallpaper` starts a new one if it gives it
            // back.
            if closed {
                tracing::info!(
                    "background: {command:?} on {name} closed for the wallpaper program"
                );
                // And if the wallpaper program has already gone in the
                // meantime — a swaybg killed a moment after it started, which
                // is what changing wallpaper with a restart looks like — the
                // position is free and nothing else would notice.
                if let Some(output) = self.output_by_name(&name) {
                    let output = output.clone();
                    self.background_reclaim_wallpaper(&output);
                }
                continue;
            }

            // The monitor went while the process was on its way out. Nothing
            // to put a terminal on.
            if self.output_by_name(&name).is_none() {
                continue;
            }

            let now = std::time::Instant::now();
            let fresh = window.is_none_or(|started| now.duration_since(started) > RESTART_WINDOW);
            let attempt = if fresh { 1 } else { restarts + 1 };

            if attempt > RESTART_LIMIT {
                tracing::error!(
                    "background: {command:?} on {name} exited with {status} — {attempt} times in \
                     under {}s, so it will not be started again",
                    RESTART_WINDOW.as_secs()
                );
                continue;
            }

            tracing::warn!(
                "background: {command:?} on {name} exited with {status}; restarting it \
                 ({attempt}/{RESTART_LIMIT})"
            );
            if let Err(e) = self.start_background_terminal(&name, &command) {
                tracing::error!("background: could not restart it on {name}: {e:#}");
                continue;
            }
            // The budget belongs to the run of crashes, not to the process.
            if let Some(background) = self
                .background_terminals
                .iter_mut()
                .find(|background| background.output == name)
            {
                background.restarts = attempt;
                background.restart_window = Some(if fresh { now } else { window.unwrap_or(now) });
            }
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
