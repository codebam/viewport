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
use smithay::backend::renderer::utils::DamageBag;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle};
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
    /// Which shell this is, matching `ClientState::shell_id` on its connection.
    id: u32,
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
    /// Where in the output layout this page lives.
    ///
    /// The whole layout for the desktop shell on its own, which is what it has
    /// always been. With `--url` and more than one monitor it is one screen's
    /// rectangle: see `plan_shells`.
    pub region: Rectangle<i32, Logical>,
    /// Whether this is the shell that runs the desktop.
    ///
    /// The one that is sent window events, given the keyboard by
    /// `shell.focus`, and whose overlays are drawn above the windows. A page
    /// named by `--url` alongside it is not: it is a web page on a monitor and
    /// nothing else, and telling it to lay windows out would be telling it in a
    /// language it does not speak.
    pub desktop: bool,
    /// The buffer it last committed, and its size.
    ///
    /// Per shell rather than one for the compositor, because two of them draw
    /// two different pages at two different places.
    pub owned: Option<(Dmabuf, smithay::utils::Size<i32, smithay::utils::Physical>)>,
    /// What changed in that buffer since the last frame. See
    /// `ViewportState::shell_damage` for why an element needs one at all.
    pub damage: DamageBag<i32, smithay::utils::Buffer>,
    /// This page's render element id, stable for as long as the process is.
    pub element_id: smithay::backend::renderer::element::Id,
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
    pub fn start_client_shell(
        &mut self,
        url: &str,
        region: Rectangle<i32, Logical>,
        desktop: bool,
    ) -> Result<()> {
        self.start_client_shell_degraded(url, region, desktop, false)
    }

    /// The same, with WebKit's own DMA-BUF renderer turned off.
    ///
    /// Only ever reached because the last shell process asked for it: WebKit's
    /// web process allocating through this compositor's `linux-dmabuf` has
    /// been seen to crash on the nested backend, and a desktop that comes back
    /// with one more copy inside WebKit is better than one that does not come
    /// back. The window's own buffer is a DMA-BUF either way, so nothing about
    /// the handoff to the compositor changes.
    pub fn start_client_shell_degraded(
        &mut self,
        url: &str,
        region: Rectangle<i32, Logical>,
        desktop: bool,
        degraded: bool,
    ) -> Result<()> {
        let program = self.shell_backend.shell_program().ok_or_else(|| {
            anyhow!(
                "{} does not run in a process of its own",
                self.shell_backend
            )
        })?;
        let binary = shell_binary(program)?;
        let (ours, theirs) = UnixStream::pair().context("making a socket for the shell")?;

        let id = self.next_shell_id;
        self.next_shell_id += 1;

        // The handle is dropped on purpose: what marks the connection is the
        // data behind it, which every surface on it carries and which nothing
        // outside this function can set.
        self.display_handle
            .insert_client(
                ours,
                Arc::new(ClientState {
                    shell: true,
                    shell_id: Some(id),
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
        if std::env::var_os("VIEWPORT_SHELL_WAYLAND_DEBUG").is_some() {
            command.env("WAYLAND_DEBUG", "1");
        }
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
            "shell: started {} as pid {} on {url} at {}x{}+{}+{}{}",
            binary.display(),
            child.id(),
            region.size.w,
            region.size.h,
            region.loc.x,
            region.loc.y,
            if desktop { "" } else { " (page only)" }
        );

        self.shell_clients.push(ClientShell {
            id,
            child,
            url: url.to_owned(),
            toplevel: None,
            configured: None,
            restarts: 0,
            restart_window: None,
            degraded,
            region,
            desktop,
            owned: None,
            damage: Default::default(),
            element_id: smithay::backend::renderer::element::Id::new(),
        });
        Ok(())
    }

    /// Start the shell process, if that is the backend in use.
    ///
    /// Called from both backends once the outputs exist, and does nothing for
    /// the in-process engine — which is what keeps the two call sites free of
    /// any knowledge of which one is running.
    pub fn start_shell_process(&mut self) {
        if !self.shell_backend.is_out_of_process() || !self.shell_clients.is_empty() {
            return;
        }
        for planned in self.plan_shells() {
            if let Err(e) = self.start_client_shell(&planned.url, planned.region, planned.desktop) {
                // Not fatal. Windows still map, the control socket still
                // answers, and the log says why the desktop behind them is
                // empty.
                tracing::error!("the shell did not start, so this is windows only: {e:#}");
            }
        }
    }

    /// The page named on the command line or in the config file, if any.
    ///
    /// `None` means "nothing was asked for", which is not the same as the
    /// shipped shell being asked for by name: only the first turns the second
    /// monitor into a desktop of its own.
    pub fn requested_url(&self) -> Option<String> {
        self.shell_url
            .clone()
            .or_else(|| std::env::var("VIEWPORT_SHELL_URL").ok())
    }

    /// Which pages to run, where, and which of them is the desktop.
    pub fn plan_shells(&self) -> Vec<PlannedShell> {
        let screens: Vec<Rectangle<i32, Logical>> = self
            .space
            .outputs()
            .filter_map(|output| self.space.output_geometry(output))
            .collect();
        let (width, height) = self.layout_size();
        let layout = Rectangle::from_size((width as i32, height as i32).into());
        plan_shells(
            self.requested_url().as_deref(),
            &screens,
            layout,
            self.shell_url_spans,
        )
    }

    /// Whether a surface belongs to the shell's connection.
    pub fn is_shell_client(&self, surface: &WlSurface) -> bool {
        use smithay::reexports::wayland_server::Resource as _;
        surface
            .client()
            .and_then(|client| client.get_data::<ClientState>().map(|data| data.shell))
            .unwrap_or(false)
    }

    /// Which shell a surface belongs to, by the connection it arrived on.
    pub fn shell_index_for(&self, surface: &WlSurface) -> Option<usize> {
        use smithay::reexports::wayland_server::Resource as _;
        let id = surface.client().and_then(|client| {
            client
                .get_data::<ClientState>()
                .and_then(|data| data.shell_id)
        })?;
        self.shell_clients.iter().position(|shell| shell.id == id)
    }

    /// Take the shell's toplevel, instead of letting it become a window.
    ///
    /// A window would be tiled, announced to the shell as something to lay
    /// out, listed in the taskbar and offered as a screen-share source. The
    /// desktop is none of those.
    pub fn adopt_shell_toplevel(&mut self, toplevel: ToplevelSurface) {
        let Some(at) = self.shell_index_for(toplevel.wl_surface()) else {
            tracing::warn!("a shell toplevel arrived on a connection no shell is using");
            return;
        };
        tracing::info!("shell {at}: its toplevel arrived");
        self.shell_clients[at].toplevel = Some(toplevel);
        self.shell_clients[at].configured = None;
        self.configure_client_shell();
        self.announce_shell_outputs();
    }

    /// Tell the shell which screens it is on.
    ///
    /// `wl_surface.enter`, which nothing else was sending it: the shell is not
    /// in the `Space` and is not in a layer map, so neither of the two things
    /// that normally do this ever sees it.
    ///
    /// It is not a formality. A client learns the refresh rate from the output
    /// it has entered, and one that has entered none has to guess — Chromium
    /// guesses 60Hz and paints at 60Hz for ever, on a 120Hz panel and on a
    /// 240Hz one, which measured as the shell painting a fifth as often as the
    /// WebKit backends and read as an engine being slow. It was this.
    ///
    /// The screens each page covers, so a client can learn its refresh rate.
    ///
    /// The whole layout for a desktop on its own — it is on every one of them
    /// by construction — and the screens its rectangle touches for a page that
    /// was given one monitor.
    pub fn announce_shell_outputs(&mut self) {
        let outputs: Vec<_> = self
            .space
            .outputs()
            .filter_map(|output| Some((output.clone(), self.space.output_geometry(output)?)))
            .collect();
        for shell in &self.shell_clients {
            let Some(surface) = shell.surface() else {
                continue;
            };
            for (output, geometry) in &outputs {
                if geometry.overlaps_or_touches(shell.region) {
                    output.enter(surface);
                } else {
                    output.leave(surface);
                }
            }
        }
    }

    /// Tell each page how big it is.
    ///
    /// Its own rectangle, which for a desktop on its own is the whole layout —
    /// the page is one document across every screen, so a second monitor makes
    /// the page wider rather than making a second page. A `--url` page beside
    /// it gets one monitor's worth instead, which is the whole point of it
    /// being a second process.
    pub fn configure_client_shell(&mut self) {
        let (width, height) = self.layout_size();
        if width == 0 || height == 0 {
            return;
        }
        // Regions first: an output that came or went changes what each page
        // covers, and a page configured to the size it had is a page drawn at
        // the wrong size for the rest of the session.
        self.replan_shell_regions();

        for (at, shell) in self.shell_clients.iter_mut().enumerate() {
            let size = (
                shell.region.size.w.max(0) as u32,
                shell.region.size.h.max(0) as u32,
            );
            if size.0 == 0 || size.1 == 0 || shell.configured == Some(size) {
                continue;
            }
            let Some(toplevel) = shell.toplevel.as_ref() else {
                // Configured when it arrives instead. `configured` is left
                // unset, so this is not remembered as done.
                continue;
            };
            toplevel.with_pending_state(|pending| {
                pending.size = Some((size.0 as i32, size.1 as i32).into());
                // Fullscreen and activated: a client that believes it is a
                // window draws a shadow around itself and leaves a gap at the
                // edge of the screen, and one that believes it is unfocused
                // draws itself greyed out. Neither is a thing the desktop can
                // be.
                use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
                pending.states.set(State::Fullscreen);
                pending.states.set(State::Activated);
            });
            // Only once the client has made its initial commit. The protocol
            // has the client commit an empty surface first and the compositor
            // answer with a configure; a configure sent before that is one the
            // client has no state to apply it to. The first one goes out from
            // the commit path instead, which is where every other surface here
            // gets it.
            if initial_configure_sent(toplevel) {
                tracing::info!("shell {at}: configuring it to {}x{}", size.0, size.1);
                shell.configured = Some(size);
                toplevel.send_pending_configure();
            }
        }
    }

    /// Give each running page the rectangle the current screens imply.
    ///
    /// Plugging a second monitor into a `--url` session is what this is for:
    /// the page was spanning one screen because there was one screen, and now
    /// there are two. The processes are not restarted — the plan is the same
    /// shape, only the rectangles moved — so a page that has state in it keeps
    /// it.
    fn replan_shell_regions(&mut self) {
        let planned = self.plan_shells();
        if planned.len() != self.shell_clients.len() {
            // The *number* of pages changed, which means a monitor arriving or
            // leaving turned one desktop into a page plus a desktop or back.
            // Handled by `sync_shell_processes`, which starts and stops them;
            // moving rectangles around under a plan that no longer matches
            // would put the wrong page on the wrong screen in the meantime.
            return;
        }
        for (shell, planned) in self.shell_clients.iter_mut().zip(planned) {
            shell.region = planned.region;
        }
    }

    /// Start or stop pages so that what is running matches what the screens
    /// call for.
    ///
    /// Called when the output layout changes. A session that came up on one
    /// monitor with `--url` is running one page and wants two once a second
    /// monitor arrives; one that loses a monitor wants one again.
    pub fn sync_shell_processes(&mut self) {
        if !self.shell_backend.is_out_of_process() || self.shell_clients.is_empty() {
            return;
        }
        let planned = self.plan_shells();
        let same = planned.len() == self.shell_clients.len()
            && planned
                .iter()
                .zip(&self.shell_clients)
                .all(|(planned, shell)| {
                    planned.url == shell.url && planned.desktop == shell.desktop
                });
        if same {
            self.configure_client_shell();
            return;
        }

        tracing::info!(
            "shell: the screens changed, so the desktop is now {} page(s) rather than {}",
            planned.len(),
            self.shell_clients.len()
        );

        // Reconciled by the page each process is showing, not rebuilt.
        //
        // A monitor arriving turns one page into two, and the page that was
        // already up is one of the two — the same document, at a new size, and
        // on a single-screen session it was also the desktop and now is not.
        // None of that needs a new process: the size is a configure and the
        // role is a fact the compositor holds, not something the shell was
        // told. Restarting it anyway would reload the site, lose whatever was
        // typed into it, and flash the screen for no reason.
        let mut running = std::mem::take(&mut self.shell_clients);
        // `None` where a process has to be started, which cannot happen while
        // `running` is still being taken apart — `start_client_shell` pushes
        // onto `self.shell_clients`, and the order there is the plan's.
        let mut kept: Vec<Option<ClientShell>> = planned
            .iter()
            .map(|planned| {
                let at = running.iter().position(|shell| shell.url == planned.url)?;
                let mut shell = running.remove(at);
                shell.region = planned.region;
                shell.desktop = planned.desktop;
                // Its old size is no longer what it is, so the configure that
                // follows is not skipped as already done.
                shell.configured = None;
                Some(shell)
            })
            .collect();
        // Whatever the new plan has no place for.
        for mut shell in running {
            let _ = shell.child.kill();
            let _ = shell.child.wait();
        }
        // And whatever it has a place for that is not running yet, put back at
        // its own index so shell 0 stays the page and shell 1 the desktop.
        for (at, planned) in planned.iter().enumerate() {
            if kept[at].is_some() {
                continue;
            }
            if let Err(e) = self.start_client_shell(&planned.url, planned.region, planned.desktop) {
                tracing::error!("shell: could not start it: {e:#}");
                continue;
            }
            kept[at] = self.shell_clients.pop();
        }
        self.shell_clients = kept.into_iter().flatten().collect();
        self.needs_render = true;
    }

    /// A commit on the shell's surface: take the buffer it painted.
    ///
    /// Returns whether the surface was the shell's, which is what stops the
    /// rest of the commit path treating it as a window.
    pub fn shell_client_commit(&mut self, surface: &WlSurface) -> bool {
        let Some(at) = self
            .shell_clients
            .iter()
            .position(|shell| shell.surface() == Some(surface))
        else {
            return false;
        };

        // The initial configure, if this is the initial commit. Until it goes
        // out the client has been told nothing about how big the desktop is,
        // and a GTK window with no size of its own picks one — so the shell
        // would come up as a small page in the corner of the screen.
        let pending = self.shell_clients[at].toplevel.clone();
        if let Some(toplevel) = pending {
            if !initial_configure_sent(&toplevel) {
                self.shell_clients[at].configured = None;
                self.configure_client_shell();
                let region = self.shell_clients[at].region;
                let size = (region.size.w.max(0) as u32, region.size.h.max(0) as u32);
                tracing::info!("shell {at}: configuring it to {}x{}", size.0, size.1);
                self.shell_clients[at].configured = Some(size);
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

        // Every size the shell paints at, not only the first.
        //
        // A shell whose buffer stops matching the layout is drawn at the wrong
        // size — a desktop in the corner of the screen, or one cropped — and
        // the only place that is visible is here. It changes on a layout
        // change and otherwise never, so this is a line at startup and a line
        // when something moved.
        let shell = &mut self.shell_clients[at];
        let previous = shell.owned.as_ref().map(|(_, size)| *size);
        if previous != Some(size) {
            match previous {
                None => tracing::info!("shell {at}: first frame, {}x{}", size.w, size.h),
                Some(was) => tracing::info!(
                    "shell {at}: painting {}x{}, was {}x{}",
                    size.w,
                    size.h,
                    was.w,
                    was.h
                ),
            }
        }
        self.shell_frames += 1;

        // The whole buffer. Surface damage is in surface coordinates and the
        // shell element is drawn in buffer coordinates; the two agree for a
        // scale-1 surface and diverge for any other, and a damage rectangle
        // that is wrong is a frame that is drawn wrong rather than one that is
        // drawn slowly.
        shell.damage.add([smithay::utils::Rectangle::from_size(
            (size.w, size.h).into(),
        )]);

        // No copy, unlike the WPE path. WebKit paints into its own pool and
        // reuses a buffer the moment it is released, so that backend has to
        // copy; a Wayland client's buffer is ours until it commits the next
        // one, and the commit that replaces it is the one that replaces this.
        shell.owned = Some((dmabuf, size));
        self.needs_render = true;
        true
    }

    /// Notice the shell process dying, and start it again.
    ///
    /// Polled rather than driven by `SIGCHLD`: the compositor has no signal
    /// handling of its own, and a shell that has been gone for at most a
    /// second is not something anybody can see. Called from the slow tick.
    pub fn check_client_shell(&mut self) {
        // One at a time. Both pages dying in the same second is a real case —
        // a GPU reset takes every process using it — and restarting them one
        // tick apart costs nothing anybody can see.
        let Some(at) = self.shell_clients.iter_mut().position(|shell| {
            // Still running, or we cannot tell — either way there is nothing
            // to do, and reaping it twice is not a thing to attempt.
            matches!(shell.child.try_wait(), Ok(Some(_)))
        }) else {
            return;
        };
        let shell = &mut self.shell_clients[at];
        let Ok(Some(status)) = shell.child.try_wait() else {
            return;
        };

        let url = shell.url.clone();
        let region = shell.region;
        let desktop = shell.desktop;
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
        self.shell_clients.remove(at);
        self.shell_frames = 0;
        self.needs_render = true;

        if attempt > RESTART_LIMIT {
            tracing::error!(
                "shell {at}: exited with {status} — {attempt} times in under {}s, so it will not \
                 be started again. That page is now blank; the compositor is not",
                RESTART_WINDOW.as_secs()
            );
            return;
        }

        if degraded {
            tracing::warn!(
                "shell {at}: exited with {status}; restarting it ({attempt}/{RESTART_LIMIT}) with \
                 WebKit's DMA-BUF renderer off"
            );
        } else {
            tracing::warn!(
                "shell {at}: exited with {status}; restarting it ({attempt}/{RESTART_LIMIT})"
            );
        }
        if let Err(e) = self.start_client_shell_degraded(&url, region, desktop, degraded) {
            tracing::error!("shell {at}: could not restart it: {e:#}");
            return;
        }
        // Back where it was, so shell 0 stays the page and shell 1 stays the
        // desktop — the plan is positional and a restart must not reorder it.
        let restarted = self.shell_clients.pop().expect("just pushed");
        self.shell_clients.insert(at, restarted);
        // The budget belongs to the run of crashes, not to the process, so it
        // is carried across the restart that the new process inherits.
        self.shell_clients[at].restarts = attempt;
        self.shell_clients[at].restart_window = Some(now);
    }

    /// The desktop shell's surface, for hit-testing and focus.
    ///
    /// The one that runs the desktop, which is the only one `shell.focus` and
    /// the window protocol mean anything to. A `--url` page beside it is a web
    /// page and nothing more.
    pub fn shell_client_surface(&self) -> Option<&WlSurface> {
        self.shell_clients
            .iter()
            .find(|shell| shell.desktop)
            .and_then(|shell| shell.surface())
    }

    /// Every shell surface, for the things that are owed to all of them:
    /// frame callbacks, presentation feedback, fifo barriers.
    pub fn shell_client_surfaces(&self) -> Vec<WlSurface> {
        self.shell_clients
            .iter()
            .filter_map(|shell| shell.surface().cloned())
            .collect()
    }

    /// The page under a point in layout coordinates, and where its top-left is.
    pub fn shell_at(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        // Last first, so a page started later is on top where two overlap. They
        // do not overlap in any plan this produces, but nothing stops a
        // rectangle being stale for a frame after a monitor moved.
        self.shell_clients.iter().rev().find_map(|shell| {
            let region = shell.region.to_f64();
            if !region.contains(pos) {
                return None;
            }
            Some((shell.surface()?.clone(), region.loc))
        })
    }

    /// Forget a shell's toplevel when it goes.
    ///
    /// The process may still be alive — a client can destroy a toplevel and
    /// make another — so this drops the surface and not the shell.
    pub fn shell_toplevel_destroyed(&mut self, surface: &WlSurface) -> bool {
        let Some(at) = self
            .shell_clients
            .iter()
            .position(|shell| shell.surface() == Some(surface))
        else {
            return false;
        };
        tracing::info!("shell {at}: its toplevel went away");
        let shell = &mut self.shell_clients[at];
        shell.toplevel = None;
        shell.configured = None;
        shell.owned = None;
        self.needs_render = true;
        true
    }
}

/// One page to run: what to load, where to put it, and what it is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedShell {
    pub url: String,
    pub region: Rectangle<i32, Logical>,
    /// Whether this one runs the desktop. Exactly one plan has it set.
    pub desktop: bool,
}

/// Work out which pages to run from the screens there are and the page asked
/// for.
///
/// Three shapes, and the middle one is the whole point of this function:
///
/// * Nothing asked for — one desktop across every screen, which is what this
///   compositor has always done. `--url` pointing at a shell under development
///   is the same case: it *is* the desktop, it just is not the shipped one.
///
/// * A page asked for, on a machine with one screen — the same again. There is
///   nowhere else to put a desktop, and a page with the whole screen and no
///   window manager is a kiosk, which is a thing people ask for on purpose.
///
/// * A page asked for, on a machine with two screens or more — the page gets
///   the *first* screen and the shipped desktop gets the rest. `--url
///   https://example.com` then means "that site on my main monitor, my desktop
///   on the others" rather than "that site stretched across every monitor,
///   and no desktop anywhere".
///
/// First by detection order, not by position: `space.outputs()` is in the order
/// the outputs were announced, and that is the order the session came up in.
/// Sorting by x would make plugging a monitor in on the left move the page.
///
/// `spans` puts the second case back for the third: a shell being developed on
/// a two-monitor desk still wants both, and it is what `--url-span` is for.
pub fn plan_shells(
    url: Option<&str>,
    screens: &[Rectangle<i32, Logical>],
    layout: Rectangle<i32, Logical>,
    spans: bool,
) -> Vec<PlannedShell> {
    let default = || crate::state::shipped_asset("shell/index.html");
    let Some(url) = url else {
        return vec![PlannedShell {
            url: default(),
            region: layout,
            desktop: true,
        }];
    };

    // The rest of the layout, as one rectangle. Two screens side by side make a
    // rectangle; an L-shaped arrangement makes one with a corner of nothing in
    // it, and the desktop page is drawn under everything so nobody sees the
    // corner it painted where no screen is.
    let rest = screens
        .iter()
        .skip(1)
        .copied()
        .reduce(|a: Rectangle<i32, Logical>, b| a.merge(b));

    match rest {
        Some(rest) if !spans => vec![
            PlannedShell {
                url: url.to_owned(),
                region: screens[0],
                desktop: false,
            },
            PlannedShell {
                url: default(),
                region: rest,
                desktop: true,
            },
        ],
        // One screen, or asked to span every one of them.
        _ => vec![PlannedShell {
            url: url.to_owned(),
            region: layout,
            desktop: true,
        }],
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

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(x: i32, w: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, 0).into(), (w, 1080).into())
    }

    #[test]
    fn no_page_asked_for_is_one_desktop_across_everything() {
        let screens = [screen(0, 1920), screen(1920, 2560)];
        let layout = Rectangle::from_size((4480, 1080).into());
        let plan = plan_shells(None, &screens, layout, false);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].region, layout);
        assert!(plan[0].desktop);
    }

    #[test]
    fn a_page_on_one_screen_is_the_desktop() {
        // Nowhere to put a second one, and a page with the whole screen and no
        // window manager is a kiosk rather than a mistake.
        let screens = [screen(0, 1920)];
        let layout = Rectangle::from_size((1920, 1080).into());
        let plan = plan_shells(Some("https://example.com"), &screens, layout, false);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].url, "https://example.com");
        assert!(plan[0].desktop);
    }

    #[test]
    fn a_page_on_two_screens_takes_the_first_and_leaves_the_desktop_the_rest() {
        let screens = [screen(0, 1920), screen(1920, 2560)];
        let layout = Rectangle::from_size((4480, 1080).into());
        let plan = plan_shells(Some("https://example.com"), &screens, layout, false);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].url, "https://example.com");
        assert_eq!(plan[0].region, screens[0]);
        assert!(!plan[0].desktop, "a web page does not run the desktop");
        assert_eq!(plan[1].region, screens[1]);
        assert!(plan[1].desktop);
        assert_ne!(plan[1].url, plan[0].url, "the desktop is the shipped shell");
    }

    #[test]
    fn the_desktop_gets_every_screen_the_page_did_not() {
        let screens = [screen(0, 1920), screen(1920, 1280), screen(3200, 1280)];
        let layout = Rectangle::from_size((4480, 1080).into());
        let plan = plan_shells(Some("https://example.com"), &screens, layout, false);
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[1].region,
            Rectangle::new((1920, 0).into(), (2560, 1080).into())
        );
    }

    #[test]
    fn spanning_puts_the_page_back_across_the_whole_desk() {
        // What a shell being developed wants: it is the desktop, it just is not
        // the shipped one.
        let screens = [screen(0, 1920), screen(1920, 2560)];
        let layout = Rectangle::from_size((4480, 1080).into());
        let plan = plan_shells(Some("http://localhost:3000"), &screens, layout, true);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].region, layout);
        assert!(plan[0].desktop);
    }

    #[test]
    fn every_plan_has_exactly_one_desktop() {
        // Nothing downstream copes with two: `shell_client_surface` answers
        // with the first, so a second would be a page that quietly receives the
        // keyboard and half the window protocol.
        let screens = [screen(0, 1920), screen(1920, 2560)];
        let layout = Rectangle::from_size((4480, 1080).into());
        for url in [None, Some("https://example.com")] {
            for spans in [false, true] {
                let plan = plan_shells(url, &screens, layout, spans);
                assert_eq!(
                    plan.iter().filter(|p| p.desktop).count(),
                    1,
                    "{url:?} {spans}"
                );
            }
        }
    }
}
