// SPDX-License-Identifier: GPL-3.0-or-later
//
// The shell in a process of its own.
//
// The WPE backend embeds the engine: the compositor owns WebKit, drives its
// main loop, translates every input event into an engine call and receives a
// DMA-BUF per frame. That is `shell.rs`, and it is most of the compositor's
// shell-specific machinery.
//
// This backend owns none of that. The shell is a Wayland client — it paints
// into a buffer and attaches it to a surface, it receives `wl_pointer` and
// `wl_keyboard` events, and it is paced by `wl_surface::frame` — so the parts
// of the compositor that exist because the shell was *not* a client mostly do
// not run at all.
//
// What is left is here, and it is three things:
//
// 1. **Identity.** The shell is not recognised by `app_id`, which any client
//    can claim. The compositor creates the Wayland connection itself and hands
//    the far end to the process it spawned, so "is this the shell" is a
//    property of the connection and cannot be forged by anything that
//    connected the ordinary way.
//
// 2. **Placement.** The shell is one page across the whole output layout, not
//    one window on one output — the same rectangle the WPE backend renders
//    into. So its toplevel is configured to the layout's size and its buffer
//    is drawn through the existing shell element, below every window, rather
//    than being mapped into the `Space` as an ordinary window.
//
// 3. **Lifetime.** It can die without taking the compositor with it, which is
//    new. The restart budget is the same shape as the WPE one in `shell.rs`:
//    a desktop that has crashed five times over a week is healthy, and one
//    that crashes five times in five seconds is a page that cannot load.
//
// Drawing the client's own buffer rather than compositing its surface is what
// makes the rest free. `frame.shell` already draws one buffer under
// everything, crops pieces of it back on top for `shell.overlay`, and copies
// it between GPUs when the import fails — all of which would have to be
// written a second time for a surface element, and none of which cares where
// the DMA-BUF came from.

use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::shell::xdg::ToplevelSurface;

use crate::state::{ClientState, ViewportState};

/// How many restarts in a window before the shell is left down.
///
/// The same policy as the WPE backend's, for the same reason — see
/// `crate::shell::budget`. Kept separately rather than shared because that
/// module only exists when the `wpe` feature is on, and this backend is the
/// one that is always compiled.
const RESTART_LIMIT: u32 = 5;

/// The status `viewport-shell-gtk` exits with when WebKit's web process has
/// crashed enough times to be a fault, and it wants starting again with
/// WebKit's DMA-BUF renderer off. Defined on both sides of the fork; see
/// `RETRY_WITHOUT_DMABUF` there.
const RETRY_WITHOUT_DMABUF: i32 = 87;
const RESTART_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// The shell process, and everything that connects it to the compositor.
pub struct ClientShell {
    child: Child,
    /// The page it was started on, so a restart loads the same thing.
    url: String,
    /// Its toplevel, once it has made one.
    toplevel: Option<ToplevelSurface>,
    /// The size it was last configured to, so a layout change that does not
    /// alter it costs no configure.
    configured: Option<(u32, u32)>,
    restarts: u32,
    restart_window: Option<std::time::Instant>,
    /// Whether this one was started with WebKit's DMA-BUF renderer off,
    /// because the last one asked for that.
    degraded: bool,
}

impl ClientShell {
    /// The shell's surface, if it has one mapped.
    pub fn surface(&self) -> Option<&WlSurface> {
        self.toplevel.as_ref().map(|toplevel| toplevel.wl_surface())
    }
}

impl ViewportState {
    /// Start the shell process and give it a connection.
    ///
    /// The order matters: the client is inserted into the display before the
    /// process exists, so there is no window in which the shell can connect
    /// and be taken for an ordinary client.
    pub fn start_client_shell(&mut self, url: &str) -> Result<()> {
        self.start_client_shell_degraded(url, false)
    }

    /// The same, with WebKit's own DMA-BUF renderer turned off.
    ///
    /// Only ever reached because the last shell process asked for it: WebKit's
    /// web process allocating through this compositor's `linux-dmabuf` has
    /// been seen to crash on the nested backend, and a desktop that comes back
    /// with one more copy inside WebKit is better than one that does not come
    /// back. The window's own buffer is a DMA-BUF either way, so nothing about
    /// the handoff to the compositor changes.
    pub fn start_client_shell_degraded(&mut self, url: &str, degraded: bool) -> Result<()> {
        let program = self.shell_backend.shell_program().ok_or_else(|| {
            anyhow!(
                "{} does not run in a process of its own",
                self.shell_backend
            )
        })?;
        let binary = shell_binary(program)?;
        let (ours, theirs) = UnixStream::pair().context("making a socket for the shell")?;

        // The handle is dropped on purpose: what marks the connection is the
        // data behind it, which every surface on it carries and which nothing
        // outside this function can set.
        self.display_handle
            .insert_client(
                ours,
                Arc::new(ClientState {
                    shell: true,
                    ..Default::default()
                }),
            )
            .map_err(|e| anyhow!("inserting the shell's connection: {e}"))?;

        // The child inherits the fd by number. `UnixStream` is close-on-exec,
        // which is right for every other fd we hold and exactly wrong for this
        // one, so the flag is cleared between fork and exec — the one place it
        // can be cleared without racing every other thread's `spawn`.
        let raw = theirs.as_raw_fd();
        let mut command = Command::new(&binary);
        command
            .env("WAYLAND_SOCKET", raw.to_string())
            // Both, and the order libwayland checks them in is the point.
            //
            // `wl_display_connect` prefers `WAYLAND_SOCKET`, takes the fd, and
            // unsets the variable — so the shell's own connection is the one
            // made for it, and it is the one carrying the flag that says this
            // client is the desktop.
            //
            // Its children then find only `WAYLAND_DISPLAY` and connect the
            // ordinary way, which is exactly right: WebKit's web process and
            // GPU process are not the shell, they only need a display to
            // allocate against. Removing the name left them with no way to
            // reach a compositor at all.
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .env("VIEWPORT_SHELL_URL", url)
            .env("VIEWPORT_IPC_SOCKET", self.ipc.path())
            // GTK picks X11 when `DISPLAY` is set, and under Xwayland the
            // shell would be a client of a client.
            .env("GDK_BACKEND", "wayland");
        if degraded {
            command.env("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }

        // SAFETY: `fcntl` is async-signal-safe, which is the whole requirement
        // on a `pre_exec` closure. Nothing here allocates or takes a lock.
        unsafe {
            use std::os::unix::process::CommandExt as _;
            command.pre_exec(move || {
                let fd = std::os::fd::BorrowedFd::borrow_raw(raw);
                smithay::reexports::rustix::io::fcntl_setfd(
                    fd,
                    smithay::reexports::rustix::io::FdFlags::empty(),
                )
                .map_err(std::io::Error::from)
            });
        }

        let child = command
            .spawn()
            .with_context(|| format!("starting {}", binary.display()))?;
        // Ours is now the child's. Holding it open would mean the compositor
        // never sees the connection close when the shell dies.
        drop(theirs);

        tracing::info!(
            "shell: started {} as pid {} on {url}",
            binary.display(),
            child.id()
        );

        self.shell_client = Some(ClientShell {
            child,
            url: url.to_owned(),
            toplevel: None,
            configured: None,
            restarts: 0,
            restart_window: None,
            degraded,
        });
        Ok(())
    }

    /// Start the shell process, if that is the backend in use.
    ///
    /// Called from both backends once the outputs exist, and does nothing for
    /// the in-process engine — which is what keeps the two call sites free of
    /// any knowledge of which one is running.
    pub fn start_shell_process(&mut self) {
        if !self.shell_backend.is_out_of_process() || self.shell_client.is_some() {
            return;
        }
        // The same order the WPE backend resolves it in: the config file, then
        // the environment, then the copy shipped beside the binary.
        let url = self
            .shell_url
            .clone()
            .or_else(|| std::env::var("VIEWPORT_SHELL_URL").ok())
            .unwrap_or_else(|| crate::state::shipped_asset("shell/index.html"));

        if let Err(e) = self.start_client_shell(&url) {
            // Not fatal. Windows still map, the control socket still answers,
            // and the log says why the desktop behind them is empty.
            tracing::error!("the shell did not start, so this is windows only: {e:#}");
        }
    }

    /// Whether a surface belongs to the shell's connection.
    pub fn is_shell_client(&self, surface: &WlSurface) -> bool {
        use smithay::reexports::wayland_server::Resource as _;
        surface
            .client()
            .and_then(|client| client.get_data::<ClientState>().map(|data| data.shell))
            .unwrap_or(false)
    }

    /// Take the shell's toplevel, instead of letting it become a window.
    ///
    /// A window would be tiled, announced to the shell as something to lay
    /// out, listed in the taskbar and offered as a screen-share source. The
    /// desktop is none of those.
    pub fn adopt_shell_toplevel(&mut self, toplevel: ToplevelSurface) {
        tracing::info!("shell: its toplevel arrived");
        if let Some(shell) = self.shell_client.as_mut() {
            shell.toplevel = Some(toplevel);
            shell.configured = None;
        }
        self.configure_client_shell();
    }

    /// Tell the shell how big the desktop is.
    ///
    /// The same size the WPE backend resizes its view to, and for the same
    /// reason: the page is one document across the whole layout, so a second
    /// monitor makes the page wider rather than making a second page.
    pub fn configure_client_shell(&mut self) {
        let size = self.layout_size();
        if size.0 == 0 || size.1 == 0 {
            return;
        }
        let Some(shell) = self.shell_client.as_mut() else {
            return;
        };
        if shell.configured == Some(size) {
            return;
        }
        let Some(toplevel) = shell.toplevel.as_ref() else {
            // Configured when it arrives instead. `configured` is left unset,
            // so this is not remembered as done.
            return;
        };
        toplevel.with_pending_state(|pending| {
            pending.size = Some((size.0 as i32, size.1 as i32).into());
            // Fullscreen and activated: a client that believes it is a window
            // draws a shadow around itself and leaves a gap at the edge of the
            // screen, and one that believes it is unfocused draws itself
            // greyed out. Neither is a thing the desktop can be.
            use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
            pending.states.set(State::Fullscreen);
            pending.states.set(State::Activated);
        });
        // Only once the client has made its initial commit. The protocol has
        // the client commit an empty surface first and the compositor answer
        // with a configure; a configure sent before that is one the client has
        // no state to apply it to. The first one goes out from the commit
        // path instead, which is where every other surface here gets it.
        if initial_configure_sent(toplevel) {
            tracing::info!("shell: configuring it to {}x{}", size.0, size.1);
            shell.configured = Some(size);
            toplevel.send_pending_configure();
        }
    }

    /// A commit on the shell's surface: take the buffer it painted.
    ///
    /// Returns whether the surface was the shell's, which is what stops the
    /// rest of the commit path treating it as a window.
    pub fn shell_client_commit(&mut self, surface: &WlSurface) -> bool {
        let is_shell = self
            .shell_client
            .as_ref()
            .and_then(|shell| shell.surface())
            .is_some_and(|shell| shell == surface);
        if !is_shell {
            return false;
        }

        // The initial configure, if this is the initial commit. Until it goes
        // out the client has been told nothing about how big the desktop is,
        // and a GTK window with no size of its own picks one — so the shell
        // would come up as a small page in the corner of the screen.
        let pending = self
            .shell_client
            .as_ref()
            .and_then(|shell| shell.toplevel.clone());
        if let Some(toplevel) = pending {
            if !initial_configure_sent(&toplevel) {
                if let Some(shell) = self.shell_client.as_mut() {
                    shell.configured = None;
                }
                self.configure_client_shell();
                let size = self.layout_size();
                tracing::info!("shell: configuring it to {}x{}", size.0, size.1);
                if let Some(shell) = self.shell_client.as_mut() {
                    shell.configured = Some(size);
                }
                toplevel.send_configure();
            }
        }

        use smithay::backend::allocator::Buffer as _;
        use smithay::backend::renderer::utils::with_renderer_surface_state;

        let buffer = with_renderer_surface_state(surface, |state| {
            let buffer = state.buffer()?;
            match smithay::wayland::dmabuf::get_dmabuf(buffer) {
                Ok(dmabuf) => Some(Ok(dmabuf.clone())),
                Err(_) => Some(Err(())),
            }
        })
        .flatten();

        let dmabuf: Dmabuf = match buffer {
            Some(Ok(dmabuf)) => dmabuf,
            Some(Err(())) => {
                // Once per session's worth of noise avoided: this repeats at
                // the shell's frame rate for as long as it lasts.
                if !self.shell_shm_warned {
                    self.shell_shm_warned = true;
                    tracing::error!(
                        "shell: it is painting into shared memory rather than a DMA-BUF, and \
                         nothing will be drawn. Its GPU stack cannot export one — check that \
                         the shell process can reach a render node, and that GSK is not falling \
                         back to its software renderer (GSK_RENDERER=ngl to insist)"
                    );
                }
                return true;
            }
            None => return true,
        };

        let size: smithay::utils::Size<i32, smithay::utils::Physical> =
            (dmabuf.width() as i32, dmabuf.height() as i32).into();

        if self.shell_owned.is_none() {
            tracing::info!("shell: first frame, {}x{}", size.w, size.h);
        }
        self.shell_frames += 1;

        // The whole buffer. Surface damage is in surface coordinates and the
        // shell element is drawn in buffer coordinates; the two agree for a
        // scale-1 surface and diverge for any other, and a damage rectangle
        // that is wrong is a frame that is drawn wrong rather than one that is
        // drawn slowly.
        self.shell_damage.add([smithay::utils::Rectangle::from_size(
            (size.w, size.h).into(),
        )]);

        // No copy, unlike the WPE path. WebKit paints into its own pool and
        // reuses a buffer the moment it is released, so that backend has to
        // copy; a Wayland client's buffer is ours until it commits the next
        // one, and the commit that replaces it is the one that replaces this.
        self.shell_owned = Some((dmabuf, size));
        self.needs_render = true;
        true
    }

    /// Notice the shell process dying, and start it again.
    ///
    /// Polled rather than driven by `SIGCHLD`: the compositor has no signal
    /// handling of its own, and a shell that has been gone for at most a
    /// second is not something anybody can see. Called from the slow tick.
    pub fn check_client_shell(&mut self) {
        let Some(shell) = self.shell_client.as_mut() else {
            return;
        };
        let status = match shell.child.try_wait() {
            Ok(Some(status)) => status,
            // Still running, or we cannot tell — either way there is nothing
            // to do, and reaping it twice is not a thing to attempt.
            Ok(None) | Err(_) => return,
        };

        let url = shell.url.clone();
        // Asked for by the process that just exited, and honoured for every
        // restart after it: turning it back on would only crash again.
        let degraded = shell.degraded || status.code() == Some(RETRY_WITHOUT_DMABUF);
        let now = std::time::Instant::now();
        let fresh = shell
            .restart_window
            .is_none_or(|started| now.duration_since(started) > RESTART_WINDOW);
        if fresh {
            shell.restart_window = Some(now);
            shell.restarts = 0;
        }
        shell.restarts += 1;
        let attempt = shell.restarts;

        // Whatever it painted belonged to the process that has gone. Leaving
        // it up would be a desktop that is a photograph: it still shows
        // windows where they were, and nothing in it can be clicked.
        self.shell_client = None;
        self.shell_owned = None;
        self.shell_frames = 0;
        self.needs_render = true;

        if attempt > RESTART_LIMIT {
            tracing::error!(
                "shell: exited with {status} — {attempt} times in under {}s, so it will not be \
                 started again. The desktop is now blank; the compositor is not",
                RESTART_WINDOW.as_secs()
            );
            return;
        }

        if degraded {
            tracing::warn!(
                "shell: exited with {status}; restarting it ({attempt}/{RESTART_LIMIT}) with \
                 WebKit's DMA-BUF renderer off"
            );
        } else {
            tracing::warn!(
                "shell: exited with {status}; restarting it ({attempt}/{RESTART_LIMIT})"
            );
        }
        if let Err(e) = self.start_client_shell_degraded(&url, degraded) {
            tracing::error!("shell: could not restart it: {e:#}");
            return;
        }
        // The budget belongs to the run of crashes, not to the process, so it
        // is carried across the restart that the new process inherits.
        if let Some(shell) = self.shell_client.as_mut() {
            shell.restarts = attempt;
            shell.restart_window = Some(now);
        }
    }

    /// The shell's surface, for hit-testing and frame callbacks.
    pub fn shell_client_surface(&self) -> Option<&WlSurface> {
        self.shell_client.as_ref().and_then(|shell| shell.surface())
    }

    /// Forget the shell's toplevel when it goes.
    ///
    /// The process may still be alive — a client can destroy a toplevel and
    /// make another — so this drops the surface and not the shell.
    pub fn shell_toplevel_destroyed(&mut self, surface: &WlSurface) -> bool {
        let Some(shell) = self.shell_client.as_mut() else {
            return false;
        };
        if shell.surface() != Some(surface) {
            return false;
        }
        tracing::info!("shell: its toplevel went away");
        shell.toplevel = None;
        shell.configured = None;
        self.shell_owned = None;
        self.needs_render = true;
        true
    }
}

/// Whether the client has been answered yet.
///
/// Smithay tracks it per toplevel, and the answer decides which of the two
/// ways of sending a configure is the legal one.
fn initial_configure_sent(toplevel: &ToplevelSurface) -> bool {
    smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
            .map(|data| data.lock().unwrap().initial_configure_sent)
            // No toplevel data at all: nothing this can usefully say, and
            // claiming "not sent" would send a second one.
            .unwrap_or(true)
    })
}

/// Where the shell process is.
///
/// Beside the compositor first, because an installed Viewport has both in the
/// same `bin` directory and a system with two versions installed must not run
/// one's compositor against the other's shell. `PATH` after that, for a
/// development tree where the binary is in `target/` and the shell is not.
fn shell_binary(name: &str) -> Result<PathBuf> {
    if let Ok(path) = std::env::var("VIEWPORT_SHELL_BIN") {
        let path = PathBuf::from(path);
        if !path.exists() {
            return Err(anyhow!(
                "VIEWPORT_SHELL_BIN names {}, which does not exist",
                path.display()
            ));
        }
        return Ok(path);
    }

    if let Some(sibling) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .filter(|path| path.exists())
    {
        return Ok(sibling);
    }

    // Not resolved here: `Command` searches `PATH` itself, and a bare name is
    // the only way to say "whatever is installed" without reimplementing that
    // search. The failure, if there is one, is reported by `spawn`.
    Ok(PathBuf::from(name))
}
