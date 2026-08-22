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
//    new. A run of crashes is told from a healthy desktop the same way the WPE
//    backend tells them apart — a desktop that has crashed five times over a
//    week is healthy, one that crashes five times in five seconds is not — but
//    what a run earns here is a growing wait rather than a shell that is left
//    down for good. See `restart_backoff`. The one exception is a shell that
//    declares it has run out of tries: exit code 88 is believed, and that slot
//    stays down. See `DEGRADED_EXHAUSTED`.
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
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{anyhow, Context as _, Result};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::utils::DamageBag;
use smithay::output::Output;
use smithay::reexports::wayland_server::backend::DisconnectReason;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::shell::xdg::ToplevelSurface;

use crate::state::{ClientState, ViewportState};

/// How many restarts in a run before they are spread out to
/// [`RESTART_SLOW`] apart.
///
/// The one home for the number: the WPE backend's budget — see
/// `crate::shell::budget` — reads these rather than keeping its own, so the
/// two cannot drift. What happens at the end of a run is still each policy's
/// own, and deliberately: this backend never gives up, that one does. See
/// [`restart_backoff`]. This module is the home because it is the one that is
/// always compiled; `shell` only exists under the `wpe` feature.
pub const RESTART_LIMIT: u32 = 5;

/// The status `viewport-shell-gtk` exits with when WebKit's web process has
/// crashed enough times to be a fault, and it wants starting again with
/// WebKit's DMA-BUF renderer off. Defined on both sides of the fork; see
/// `RETRY_WITHOUT_DMABUF` there.
const RETRY_WITHOUT_DMABUF: i32 = 87;

/// The status `viewport-shell-gtk` exits with when it has used up every try it
/// had. Its budget is per process — three web-process crashes ask to be started
/// degraded, five slow reloads are spent there, and when those have died too it
/// quits rather than reload for ever. Defined on both sides of the fork; see
/// `DEGRADED_EXHAUSTED` there, whose comment this number answers.
///
/// It is the one exit this supervisor does not answer with another turn on the
/// restart treadmill. [`restart_backoff`] never gives up, on purpose and for
/// reasons of its own; but a shell exiting 88 has been through those waits
/// already and come out the far side of degraded mode having spent everything
/// it had against the same GPU it would be started on again. Starting it once
/// more retries nothing new — it rebuilds, at whatever pace, exactly the storm
/// the shell's cap exists to end. So the code means what the shell meant by it:
/// log it loudly, take that slot down, and leave the rest of the session alone.
/// See `check_client_shell`, where the verdict is carried out.
const DEGRADED_EXHAUSTED: i32 = 88;

/// A shell that has run this long since the last crash is a healthy one, and
/// the next crash begins a new run.
///
/// Shared with the WPE budget, for the reason [`RESTART_LIMIT`] gives.
pub const RESTART_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// How far apart restarts are once the fast ones have been used up.
///
/// Under [`RESTART_WINDOW`] on purpose. The run is reset by a process that
/// lived longer than the window, and the wait before a start counts towards
/// that lifetime from the outside — so a retry interval at or above the window
/// would read every instantly-crashing shell as healthy and go back to
/// restarting it as fast as the tick allows, which is the loop this is here to
/// stop.
const RESTART_SLOW: std::time::Duration = std::time::Duration::from_secs(30);

/// The first backoff, doubled for each restart after it.
const RESTART_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// Shell children killed on purpose, waiting for the slow tick to reap them.
///
/// `sync_shell_processes` used to kill each displaced shell and then block in
/// a `wait` on it — on the event loop, on every hotplug. A child stuck in
/// uninterruptible sleep never comes out of that wait, and the desktop stops
/// with the event loop inside it; a GPU hang puts every process holding its
/// buffers exactly there, and see `check_client_shell` for how real that case
/// is. So killing is all that happens here now: the pid goes on this list and
/// the slow tick asks `try_wait` once a second, which cannot block. One that
/// never answers stays listed — a zombie is cheaper than a hung compositor,
/// and the list is bounded by how many pages a session can have.
static REAPING: LazyLock<Mutex<Vec<Child>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Ask, without blocking, whether any killed shell can be reaped yet.
///
/// An error from `try_wait` means there is nothing left of the child to ask
/// about — reaped elsewhere, or already gone — so it leaves the list too.
fn reap_killed_shells() {
    let mut reaping = REAPING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reaping.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
}

/// How long a shell process is given to stop by itself at quit.
///
/// It is told by the control socket closing, which is the same end-of-session
/// it already handles, and a shell that takes longer than this is one nobody
/// is waiting on any more: the screens are about to go.
const STOP_GRACE: std::time::Duration = std::time::Duration::from_millis(800);

/// And how long after `SIGTERM` before it is killed outright.
const STOP_TERM_GRACE: std::time::Duration = std::time::Duration::from_millis(300);

/// How often a stopping shell is looked at. Short enough that the common case
/// — a shell that exits in a few milliseconds — costs nothing measurable.
const STOP_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// What to do about a shell process that has just died: which attempt of the
/// current run it is, and how long to wait before making it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restart {
    pub attempt: u32,
    pub delay: std::time::Duration,
}

/// Decide when to start the shell again, and update the run.
///
/// A plain restart limit is wrong in both directions: a desktop up for a week
/// that has crashed five times over that week is healthy, and one that crashes
/// five times in five seconds is a page that cannot load. [`RESTART_WINDOW`]
/// is what tells them apart — a crash far enough after the last one starts a
/// new run.
///
/// Within a run the wait doubles. The first restart is immediate, because a
/// shell that has crashed once is a shell that has crashed once and a second
/// of blank desktop would buy nothing; every one after it waits longer, on the
/// grounds that a process dying repeatedly in a few seconds is a fault that
/// starting it again *right now* will not fix.
///
/// It never gives up, which is the difference from the WPE budget. That policy
/// was written for a page that cannot load, where retrying is an infinite loop
/// that forks — but the fault that reaches here first is not that one. The
/// shell died five times in ninety seconds because the GPU had run out of
/// memory with a game, a screen capture and a language model on it, and every
/// other client on that GPU was dying the same way; the desktop then stayed
/// blank for the rest of the session, long after closing the game would have
/// fixed it. A page that cannot load costs one blank shell and one log line
/// every [`RESTART_SLOW`]; a transient system-wide fault costs the desktop
/// until the session is restarted. The second is much the worse of the two, so
/// the run slows to a crawl rather than stopping.
///
/// (One exit does stop the supervisor outright, and it is not decided in this
/// function: a shell that exits 88 has declared it has nothing left to try,
/// and is believed before the arithmetic here is ever reached. See
/// [`DEGRADED_EXHAUSTED`].)
pub fn restart_backoff(
    restarts: &mut u32,
    window: &mut Option<std::time::Instant>,
    now: std::time::Instant,
) -> Restart {
    let fresh = window.is_none_or(|last| now.duration_since(last) > RESTART_WINDOW);
    if fresh {
        *restarts = 0;
    }
    *window = Some(now);
    *restarts += 1;
    let attempt = *restarts;

    // 0s, 1s, 2s, 4s, 8s, then the slow interval for as long as it keeps
    // dying. The shift is bounded by the branch above it.
    let delay = match attempt {
        _ if attempt > RESTART_LIMIT => RESTART_SLOW,
        0 | 1 => std::time::Duration::ZERO,
        _ => (RESTART_BACKOFF * (1 << (attempt - 2))).min(RESTART_SLOW),
    };
    Restart { attempt, delay }
}

/// Whether an exit status is the shell giving up rather than asking for
/// another go.
///
/// Eighty-eight is the whole of the vocabulary for it: the shell exits with it
/// only after its crash limit and every slow reload beyond that have failed,
/// so the code arrives carrying the whole argument with it, and the caller
/// need not count anything. Kept apart from [`RETRY_WITHOUT_DMABUF`], which
/// asks for one specific second chance and gets it.
fn gave_up(status: &std::process::ExitStatus) -> bool {
    status.code() == Some(DEGRADED_EXHAUSTED)
}

/// A shell that has died and is waiting out its backoff before starting again.
///
/// Held apart from `shell_clients` rather than as a dead entry in it, because
/// everything that reads that list — placement, focus, the window protocol,
/// rendering — means "the shells that are running", and a page waiting to come
/// back is not one of them.
pub struct PendingShell {
    /// The page it was showing. Both what the restart loads and how it is
    /// found in the plan again, which is where its rectangle and its role come
    /// from — those can have moved while it waited.
    pub url: String,
    /// Whether it asked to come back with WebKit's DMA-BUF renderer off.
    pub degraded: bool,
    /// Which shell it was, so shell 0 goes back to being shell 0.
    pub at: usize,
    /// The run of crashes it belongs to, carried into the process it starts.
    pub restarts: u32,
    pub restart_window: Option<std::time::Instant>,
    /// The earliest it may be started.
    pub due: std::time::Instant,
}

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
    /// The outputs its surface has been told it is on.
    ///
    /// `wl_surface.enter` and `leave` are a transition, not a statement, and
    /// toolkits count them: see [`ViewportState::announce_shell_outputs`].
    entered: Vec<Output>,
}

impl ClientShell {
    /// The shell's surface, if it has one mapped.
    pub fn surface(&self) -> Option<&WlSurface> {
        self.toplevel.as_ref().map(|toplevel| toplevel.wl_surface())
    }

    /// The process, for matching a control-socket connection to this page.
    pub fn pid(&self) -> Option<i32> {
        self.child.id().try_into().ok()
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
        // outside this function can set. The id is kept, because a spawn that
        // fails below has to take this connection back down with it.
        let client = self
            .display_handle
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

        // Whether anything is drawn behind the page.
        //
        // The engine composites the document over a colour of its own, and the
        // default is opaque — deliberately, or a shell that has not finished
        // loading flashes white across every monitor. With a terminal as the
        // wallpaper that opaque colour is what covers it, so the shell process
        // is told to composite over nothing instead. The page still has to
        // stop painting its own background; both halves are needed, and
        // `Config::background_terminal` is the other.
        if self.background_command.is_some() {
            command.env("VIEWPORT_SHELL_TRANSPARENT", "1");
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

        let child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                // The connection was made before the process existed, so a
                // failed spawn leaves it behind with nobody ever to connect
                // on it: a client slot and the fd under it, held for the rest
                // of the session. Taken back down before the error goes out.
                self.display_handle
                    .backend_handle()
                    .kill_client(client.id(), DisconnectReason::ConnectionClosed);
                return Err(e).with_context(|| format!("starting {}", binary.display()));
            }
        };
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
            entered: Vec::new(),
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
        // A new surface has entered nothing, whatever the one before it had.
        self.shell_clients[at].entered.clear();
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
    ///
    /// Sent on the transition and not otherwise. `enter` and `leave` are a
    /// change of state rather than a statement of it, and a toolkit counts
    /// them: an output entered twice and left once is still entered, and one
    /// left without having been entered — the ordinary case for a `--url` page
    /// covering screen 0 while screen 1 exists — takes the count below zero.
    /// What that costs is the scale and the refresh rate the page was drawing
    /// at, which is the whole reason any of this is sent.
    pub fn announce_shell_outputs(&mut self) {
        let outputs: Vec<_> = self
            .space
            .outputs()
            .filter_map(|output| Some((output.clone(), self.space.output_geometry(output)?)))
            .collect();
        for shell in &mut self.shell_clients {
            let Some(surface) = shell
                .toplevel
                .as_ref()
                .map(|toplevel| toplevel.wl_surface())
            else {
                continue;
            };
            for (output, geometry) in &outputs {
                let on = geometry.overlaps_or_touches(shell.region);
                let told = shell.entered.contains(output);
                if on && !told {
                    output.enter(surface);
                    shell.entered.push(output.clone());
                } else if !on && told {
                    output.leave(surface);
                    shell.entered.retain(|entered| entered != output);
                }
            }
            // An output that has gone away is not in the list above, so the
            // shell is never told it left one — and nothing else would clear
            // it, which would leave a page that reconnects to a screen of the
            // same name believing it is already on it.
            shell
                .entered
                .retain(|entered| outputs.iter().any(|(output, _)| output == entered));
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
        // Whatever the new plan has no place for. Killed here; reaped on the
        // slow tick, because a blocking `wait` on the event loop would hang
        // the whole desktop behind one child stuck in uninterruptible sleep.
        // See `REAPING`.
        for mut shell in running {
            let _ = shell.child.kill();
            REAPING
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(shell.child);
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

        // The keyboard, if nothing else wants it.
        //
        // Here rather than at `adopt_shell_toplevel`, because a surface that
        // has not committed is one `wl_keyboard.enter` would name before there
        // is anything to type into. A page that has committed is a page
        // somebody can see, and on a `--url` session it is the whole session:
        // requiring a click first would make a kiosk untypable until somebody
        // found the mouse.
        //
        // Before the buffer is looked at, so a shell painting into shared
        // memory is still typable. Nothing will be drawn for it — that is the
        // error below — but keys reaching a page nobody can see is a better
        // failure than a page that is up and deaf.
        self.focus_shell_if_idle();

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

    /// Stop every shell process, before the compositor's own sockets go.
    ///
    /// Quitting used to be a drop: the loop stopped, everything the compositor
    /// owned was dropped in declaration order, and the shell first heard about
    /// it when its Wayland connection broke mid-frame. What that produced was
    /// not a clean exit but a crash — servoshell took winit's error path, ran
    /// its exit handlers on a half-torn-down engine and died of `SIGSEGV`
    /// inside them, and the compositor then reaped a shell it had already
    /// killed.
    ///
    /// So the order is made explicit here, and it is the order the shell was
    /// written for:
    ///
    /// 1. Close the control socket. Every out-of-process backend watches it
    ///    and stops its engine when it closes — the same path as a compositor
    ///    that has genuinely gone away, except that this time the display is
    ///    still up while the engine shuts down, so nothing takes a broken-pipe
    ///    path on the way out.
    /// 2. Wait [`STOP_GRACE`] for the process to go.
    /// 3. `SIGTERM`, then [`STOP_TERM_GRACE`], then kill it. A shell that
    ///    ignores its socket closing does not get to hold up the quit.
    ///
    /// Called from `main` after the event loop returns and before the display
    /// is dropped, which is the only place both halves of that ordering exist.
    pub fn stop_client_shells(&mut self) {
        if self.shell_clients.is_empty() {
            return;
        }

        // First, because it is what the shells are waiting to hear.
        self.ipc.close_all();

        let deadline = std::time::Instant::now() + STOP_GRACE;
        while std::time::Instant::now() < deadline && self.any_shell_running() {
            std::thread::sleep(STOP_POLL);
        }

        for shell in &mut self.shell_clients {
            if matches!(shell.child.try_wait(), Ok(None)) {
                if let Some(pid) = shell.pid() {
                    tracing::info!(
                        "shell {}: still up after its socket closed; SIGTERM",
                        shell.id
                    );
                    // SAFETY: a plain `kill(2)` on a pid this process owns and
                    // has not yet reaped, so the pid cannot have been reused.
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                }
            }
        }

        let deadline = std::time::Instant::now() + STOP_TERM_GRACE;
        while std::time::Instant::now() < deadline && self.any_shell_running() {
            std::thread::sleep(STOP_POLL);
        }

        for shell in &mut self.shell_clients {
            if matches!(shell.child.try_wait(), Ok(None)) {
                tracing::warn!("shell {}: did not stop; killing it", shell.id);
                let _ = shell.child.kill();
            }
            // Reaped either way: this is the process that started it, and a
            // shell left as a zombie outlives the compositor's own exit.
            let _ = shell.child.wait();
        }
    }

    /// Whether any shell process is still alive, for the waits above.
    fn any_shell_running(&mut self) -> bool {
        self.shell_clients
            .iter_mut()
            .any(|shell| matches!(shell.child.try_wait(), Ok(None)))
    }

    /// Notice the shell process dying, and start it again.
    ///
    /// Polled rather than driven by `SIGCHLD`: the compositor has no signal
    /// handling of its own, and a shell that has been gone for at most a
    /// second is not something anybody can see. Called from the slow tick.
    pub fn check_client_shell(&mut self) {
        // First, what `sync_shell_processes` killed: reaped here rather than
        // where they died, because this tick cannot block and that path runs
        // on the event loop. See `REAPING`.
        reap_killed_shells();

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
        // Asked for by the process that just exited, and honoured for every
        // restart after it: turning it back on would only crash again.
        let degraded = shell.degraded || status.code() == Some(RETRY_WITHOUT_DMABUF);
        let now = std::time::Instant::now();
        let mut restarts = shell.restarts;
        let mut window = shell.restart_window;

        // Whatever it painted belonged to the process that has gone. Leaving
        // it up would be a desktop that is a photograph: it still shows
        // windows where they were, and nothing in it can be clicked.
        //
        // And if what it painted was the lock screen, the same applies with
        // teeth: a photograph of a lock screen accepts no password, so the
        // compositor stops drawing it and shows black until whatever comes
        // back has drawn a real one. The session stays locked throughout —
        // killing the shell is not a way past the lock.
        self.forget_lock_screen();
        self.shell_clients.remove(at);
        self.shell_frames = 0;
        self.needs_render = true;

        // A shell that has given up gets neither a restart nor the arithmetic
        // that schedules one; the run's counters above are simply dropped,
        // because there is no process after this one to carry them into.
        if gave_up(&status) {
            tracing::error!(
                "shell {at}: exited with {status}; it has given up — the web-process crash \
                 limit passed, every slow degraded reload died too, and there is nothing left \
                 for it to try against this GPU. It will not be restarted; the session goes \
                 on around it"
            );
            // Not queued, and nothing else starts shells on a clock: the slow
            // tick brings back only what sits in `pending_shells`, and this was
            // deliberately kept out of it. What can revive the page from here
            // is a person rather than a timer — `sync_shell_processes` re-runs
            // when the screens change while some other page is still up, and
            // starts any planned page that is not running, fresh process and
            // fresh budget. That is the same courtesy a config reload earns
            // once somebody has fixed their setup, and it is not the storm
            // this exit exists to stop, because nothing automatic fires
            // between human events. If this was the only page, nobody is left
            // to run even that scan, and the desktop stays down until the
            // session ends: the shell's own verdict, accepted.
            return;
        }

        let Restart { attempt, delay } = restart_backoff(&mut restarts, &mut window, now);

        if attempt > RESTART_LIMIT {
            tracing::warn!(
                "shell {at}: exited with {status} — {attempt} times, the last {} within {}s of \
                 each other, so it is now being retried every {}s instead of at once. That page \
                 is blank until one of them lives",
                RESTART_LIMIT,
                RESTART_WINDOW.as_secs(),
                delay.as_secs()
            );
        } else if degraded {
            tracing::warn!(
                "shell {at}: exited with {status}; restarting it in {}ms \
                 ({attempt}/{RESTART_LIMIT}) with WebKit's DMA-BUF renderer off",
                delay.as_millis()
            );
        } else {
            tracing::warn!(
                "shell {at}: exited with {status}; restarting it in {}ms \
                 ({attempt}/{RESTART_LIMIT})",
                delay.as_millis()
            );
        }

        self.pending_shells.push(PendingShell {
            url,
            degraded,
            at,
            restarts: attempt,
            restart_window: window,
            due: now + delay,
        });
        // A first crash still comes back on the tick it was noticed on: its
        // delay is zero, and making the desktop wait a whole tick for the
        // arithmetic to agree would be a regression dressed as a fix.
        self.start_due_shells();
    }

    /// Start whichever shell has waited out its backoff.
    ///
    /// The other half of `check_client_shell`, and called from the same slow
    /// tick. Split from it because the two happen at different times: a crash
    /// is noticed once, and the restart it earns can be up to
    /// [`RESTART_SLOW`] later.
    pub fn start_due_shells(&mut self) {
        let now = std::time::Instant::now();
        // One at a time, for the reason `check_client_shell` takes one at a
        // time: two pages dying together is a real case and starting them a
        // tick apart costs nothing anybody can see.
        let Some(index) = self
            .pending_shells
            .iter()
            .position(|pending| pending.due <= now)
        else {
            return;
        };
        let pending = self.pending_shells.remove(index);

        // The screens can move while a restart waits, and the plan is the
        // authority on both where a page goes and whether it is wanted at all:
        // a monitor unplugged during the backoff can turn a page plus a
        // desktop back into one desktop, and starting the page anyway would
        // put a second copy of it on a screen that is gone.
        let planned = self.plan_shells();
        let Some(place) = planned.iter().find(|planned| planned.url == pending.url) else {
            tracing::info!(
                "shell {}: the screens changed while it was waiting to restart, and the page it \
                 was showing is no longer in the plan, so it is not being started",
                pending.at
            );
            return;
        };
        let region = place.region;
        let desktop = place.desktop;

        let at = pending.at;
        if let Err(e) =
            self.start_client_shell_degraded(&pending.url, region, desktop, pending.degraded)
        {
            tracing::error!("shell {at}: could not restart it: {e:#}");
            // Still owed a restart: a spawn that failed is not a process that
            // lived, and dropping it here is the permanent blank desktop this
            // is meant to avoid. It goes back on the queue one slow interval
            // out rather than spinning on whatever made the spawn fail.
            self.pending_shells.push(PendingShell {
                due: now + RESTART_SLOW,
                ..pending
            });
            return;
        }
        // Back where it was, so shell 0 stays the page and shell 1 stays the
        // desktop — the plan is positional and a restart must not reorder it.
        let restarted = self.shell_clients.pop().expect("just pushed");
        let at = at.min(self.shell_clients.len());
        self.shell_clients.insert(at, restarted);
        // The budget belongs to the run of crashes, not to the process, so it
        // is carried across the restart that the new process inherits.
        self.shell_clients[at].restarts = pending.restarts;
        self.shell_clients[at].restart_window = pending.restart_window;
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

    /// Give the keyboard to a page, which is what makes a web page typable.
    ///
    /// The out-of-process shell is a Wayland client, so keys reach it the way
    /// they reach any client: `wl_keyboard.enter` and then `wl_keyboard.key`.
    /// Nothing was sending the enter. The shell bound the keyboard, received
    /// the keymap, and was never focused — so a page with a text field in it
    /// could be clicked and not typed into, and the keys were not going
    /// somewhere else, they were being intercepted and dropped: with no focused
    /// surface the key path decides they are the shell's and hands them to
    /// `shell_keyboard_key`, which posts to the in-process engine and does
    /// nothing at all when there is not one.
    ///
    /// `at` names a point in the layout — a click — and picks the page under
    /// it. Without one the desktop page is chosen, which is what a session
    /// coming up wants: the page is there, nothing else holds the keyboard, and
    /// requiring a click first would make a kiosk untypable until somebody
    /// found the mouse.
    ///
    /// False when there is nothing to focus, which is every WPE session: that
    /// engine is not a client, keys reach it through `Action::Web`, and the
    /// path above depends on the focus staying empty. The caller sets the focus
    /// to nothing then, exactly as it did before.
    pub fn focus_shell_at(&mut self, at: Option<Point<f64, Logical>>) -> bool {
        let surface = match at {
            Some(at) => self.shell_at(at).map(|(surface, _)| surface),
            None => None,
        }
        .or_else(|| self.shell_client_surface().cloned())
        // A `--url` page on its own is the desktop, so the line above finds it.
        // This is for the shape where it is not and no point was given.
        .or_else(|| self.shell_client_surfaces().into_iter().next());
        let Some(surface) = surface else {
            return false;
        };
        let Some(keyboard) = self.seat.get_keyboard() else {
            return false;
        };
        if keyboard
            .current_focus()
            .is_some_and(|focus| focus.is_surface(&surface))
        {
            return true;
        }
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, Some(surface.into()), serial);
        true
    }

    /// The same, when nothing else has the keyboard.
    ///
    /// A window that takes focus must keep it: this is the floor under an empty
    /// desktop, not a policy that competes with one.
    ///
    /// Empty means empty. A desk with a window on it and no keyboard focus is
    /// not an empty desk — it is a window that has not been focused *yet*, and
    /// the floor used to be laid straight over the top of it. The order that
    /// does it is ordinary: a client mapping before the shell is up. Focusing
    /// a window is the shell's decision and there was no shell to make it, so
    /// the window sat unfocused; the shell then started, found the seat idle
    /// and took the keyboard for itself; and the replay that followed
    /// announced the window as `replay: true` — not new, restore it where it
    /// was — which is not a thing the shell focuses, because a shell that
    /// reloaded must not steal the keyboard from whatever holds it. Three
    /// correct rules, and between them a window nothing would ever focus.
    ///
    /// It is worst for X11. A Wayland client at least gets `wl_keyboard`
    /// events once something focuses it; an X client's focus is
    /// `SetInputFocus`, which smithay only sends when an `X11Surface` is the
    /// seat's focus, so the X server sits at `PointerRoot` and delivers
    /// keystrokes to whatever the pointer is over. An autostarted X11
    /// application is the ordinary way to meet this.
    ///
    /// So the floor asks whether there is a window first, and focuses the most
    /// recently added mapped one if there is. That is what the shell would
    /// have done had it been running when the window arrived.
    pub fn focus_shell_if_idle(&mut self) {
        let idle = self
            .seat
            .get_keyboard()
            .is_some_and(|keyboard| keyboard.current_focus().is_none());
        if !idle {
            return;
        }
        // Never past a lock screen. A locked session reaches here by one
        // route — the shell crashed while it held the lock, taking the
        // keyboard with it, and its replacement's toplevel has just arrived —
        // and on that route every window on the desk is behind a lock screen.
        // Focusing one would put the keyboard into it, and the next thing
        // typed at a lock screen is a password. The shell gets it instead,
        // which is what `focus_lock_shell` would do a moment later anyway
        // when the new page asks what it is meant to be drawing.
        if !self.locked {
            // Newest rather than oldest: several windows can predate the
            // shell — a session of autostarted applications — and the last
            // one to arrive is the one a desk that had been running would
            // have focused.
            if let Some(id) = self.views.iter().filter(|v| v.mapped).map(|v| v.id).last() {
                crate::apply::focus_view(self, id);
                return;
            }
        }
        self.focus_shell_at(None);
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
        // Nothing of it is on screen any more, so nothing of it is a lock
        // screen any more. The session stays locked; see `forget_lock_screen`.
        self.forget_lock_screen();
        let shell = &mut self.shell_clients[at];
        let desktop = shell.desktop;
        shell.toplevel = None;
        shell.configured = None;
        shell.owned = None;
        self.needs_render = true;
        if desktop {
            // Its rectangles went with it. They are the shell's to report and
            // the compositor's to keep, so a page that goes — a crash and a
            // restart, a reload — leaves its last set behind, and the next
            // page only sends a list when one of *its* rectangles changes. In
            // between, this compositor draws pieces of a page that is not
            // there over the windows, and nothing on screen can dismiss them.
            self.set_shell_overlays(Vec::new(), Vec::new());
        }
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
pub(crate) fn initial_configure_sent(toplevel: &ToplevelSurface) -> bool {
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

    /// Drive `restart_backoff` over crashes at a list of offsets from a fixed
    /// start, as a run of them would arrive.
    fn run(offsets: &[std::time::Duration]) -> Vec<Restart> {
        let start = std::time::Instant::now();
        let mut restarts = 0;
        let mut window = None;
        offsets
            .iter()
            .map(|offset| restart_backoff(&mut restarts, &mut window, start + *offset))
            .collect()
    }

    #[test]
    fn a_first_crash_is_restarted_at_once() {
        // The desktop is blank until it comes back, and one crash is not
        // evidence of anything worth waiting out.
        let outcome = run(&[std::time::Duration::ZERO]);
        assert_eq!(outcome[0].attempt, 1);
        assert_eq!(outcome[0].delay, std::time::Duration::ZERO);
    }

    #[test]
    fn a_run_of_crashes_backs_off() {
        // The fault this was written for: the GPU ran out of memory, every
        // client on it died, and each restart asked it for another
        // full-screen buffer. The waits have to grow.
        let burst: Vec<_> = (0..RESTART_LIMIT)
            .map(|n| std::time::Duration::from_millis(u64::from(n) * 100))
            .collect();
        let delays: Vec<_> = run(&burst).iter().map(|r| r.delay.as_secs()).collect();
        assert_eq!(delays, vec![0, 1, 2, 4, 8]);
        assert!(delays.windows(2).all(|pair| pair[1] > pair[0]));
    }

    #[test]
    fn a_page_that_cannot_load_is_retried_slowly_rather_than_dropped() {
        // Never given up on: the difference between this and the WPE budget.
        // A page that cannot load costs one log line every RESTART_SLOW, and
        // a shell kept down through a fault that has since cleared costs the
        // desktop for the rest of the session.
        let burst: Vec<_> = (0..RESTART_LIMIT + 20)
            .map(|n| std::time::Duration::from_millis(u64::from(n) * 100))
            .collect();
        let outcome = run(&burst);
        assert!(outcome
            .iter()
            .skip(RESTART_LIMIT as usize)
            .all(|r| r.delay == RESTART_SLOW));
    }

    #[test]
    fn the_slow_retry_is_inside_the_window_that_resets_the_run() {
        // Otherwise the wait before a restart would itself look like a shell
        // that had lived a healthy life, the run would reset, and a process
        // dying on startup would be restarted as fast as the tick allows for
        // ever. See RESTART_SLOW.
        assert!(RESTART_SLOW < RESTART_WINDOW);
    }

    #[test]
    fn crashes_spread_out_never_back_off() {
        // A desktop up for a week that has crashed once a day is healthy. A
        // plain counter would have it retrying every 30s by the sixth day.
        let spread: Vec<_> = (0..20).map(|n| RESTART_WINDOW * (n + 1) * 2).collect();
        assert!(run(&spread)
            .iter()
            .all(|r| r.attempt == 1 && r.delay == std::time::Duration::ZERO));
    }

    #[test]
    fn a_shell_that_lives_starts_the_run_over() {
        // Four crashes, then a process that lasted, then four more. The
        // second run is a run of four and not the back half of one of eight.
        let mut offsets: Vec<_> = (0..4)
            .map(|n| std::time::Duration::from_millis(n * 100))
            .collect();
        let quiet = RESTART_WINDOW * 2;
        offsets.extend((0..4).map(|n| quiet + std::time::Duration::from_millis(n * 100)));
        let outcome = run(&offsets);
        assert_eq!(
            outcome.iter().map(|r| r.attempt).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 1, 2, 3, 4]
        );
    }

    #[test]
    fn eighty_eight_is_taken_as_the_shell_giving_up() {
        // The GTK shell exits 88 only once its counted, slow reloads have all
        // died too, so the code carries the whole argument with it: the slot
        // is left down rather than fed back into the restart loop. The
        // predicate is the whole decision — `check_client_shell` returns on it
        // before the backoff is even computed.
        use std::os::unix::process::ExitStatusExt as _;
        let exhausted = std::process::ExitStatus::from_raw(DEGRADED_EXHAUSTED << 8);
        assert!(gave_up(&exhausted));
        // Eighty-seven is a different request — come back degraded — and
        // still earns a restart.
        let degraded = std::process::ExitStatus::from_raw(RETRY_WITHOUT_DMABUF << 8);
        assert!(!gave_up(&degraded));
        // So does every ordinary death, a signal included: `code()` is `None`
        // for one, and the treadmill is the answer to those by design.
        let clean = std::process::ExitStatus::from_raw(0);
        assert!(!gave_up(&clean));
        let killed = std::process::ExitStatus::from_raw(6);
        assert!(!gave_up(&killed));
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
