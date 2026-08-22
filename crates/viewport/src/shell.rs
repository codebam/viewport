// SPDX-License-Identifier: GPL-3.0-or-later
//
// The shell: WebKit painting the desktop. Ports src/web.c.
//
// The compositor never touches these pixels. WebKit paints into a DMA-BUF on
// the same GPU the renderer uses, hands it over, and the buffer is imported as
// a Vulkan image — the path `viewport_vulkan::Image::import` already takes for
// client buffers, because a frame from WebKit is not special.
//
// WebKit runs on a thread of its own, with a GMainContext of its own, and the
// compositor never enters GLib at all. `WebView` and `Display` live there and
// are never touched from outside it; everything the compositor wants done to
// them is a [`Command`] posted to that thread.
//
// The other direction was already a queue and stays one. A frame callback runs
// inside WebKit underneath whatever WebKit is doing, and cannot reach into
// `&mut ViewportState` — so frames and messages land in the [`Mailbox`] and a
// calloop ping wakes the compositor to collect them.
//
// Two things follow from the split and are easy to get wrong:
//
// A `FrameToken` is an opaque `WPEBuffer` handle. It is produced on the web
// thread, travels to the compositor inside the mailbox, and comes back as
// `FrameDone` or `FrameRelease` — so it crosses threads twice. That is sound
// because nothing outside the web thread ever dereferences it: the compositor
// holds it and hands it back, and only the web thread passes it to the shim.
//
// And a command is asynchronous where the call it replaced was not. Nothing
// here returns a value from WebKit, which is what makes that safe; `restart`
// used to read `view.uri()` and now the web thread reads it, because that is
// the only thread allowed to.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::{Fourcc, Modifier};

use viewport_ipc::Event;
use viewport_web::webkit::{CrashSink, MessageSink, Termination, WebView};
use viewport_web::wpe::{Display, FrameSink, FrameToken};
use viewport_web::Frame;

// The numbers live in `shell_client`, which is compiled whether or not this
// module is; they are re-exported here because `state` reads them through
// `crate::shell`.
pub use crate::shell_client::{RESTART_LIMIT, RESTART_WINDOW};

/// A painted frame, waiting to be drawn.
pub struct Pending {
    pub buffer: Dmabuf,
    /// Returned once the frame has been presented, which is what releases
    /// WebKit to paint the next one.
    pub token: FrameToken,
}

/// What the callbacks drop things into.
///
/// A queue rather than a direct call because the callbacks run inside WebKit,
/// underneath a calloop dispatch that already holds the compositor state
/// mutably. Reaching back in from there would alias it.
#[derive(Default)]
pub struct Mailbox {
    /// Wakes the event loop once something has been posted. Without it a
    /// message would sit in the queue until unrelated input arrived, which
    /// looks exactly like a shell that has stopped responding.
    pub ping: Option<smithay::reexports::calloop::ping::Ping>,
    /// The newest frame only. An older one that has not been drawn is already
    /// out of date, and holding a queue of them would mean holding WebKit's
    /// buffers hostage.
    pub frame: Option<Pending>,
    /// Messages from the page, in order. Order matters here — the shell's
    /// layout messages are a sequence, not a state.
    pub messages: Vec<String>,
    /// Frames superseded before anything drew them. Their buffers are not in
    /// use by anyone, so they go straight back to WebKit's pool — but the
    /// callback that dropped them cannot reach the display, which is why they
    /// are queued rather than released on the spot.
    pub stale: Vec<FrameToken>,
    /// WebKit's web process died. Latest reason only: a second death before
    /// the first has been acted on says nothing new.
    pub terminated: Option<Termination>,
}

/// What the compositor asks the web thread to do.
///
/// Data only. Anything that would need a value back out of WebKit does not
/// belong here, because there is nowhere to return it to — the compositor is
/// not waiting.
enum Command {
    /// An IPC event, already serialised, for the page.
    Post(String),
    /// This frame reached the screen; WebKit may schedule the next paint.
    Done(FrameToken),
    /// Nothing samples this buffer any more; it goes back to WebKit's pool.
    Release(FrameToken),
    /// The web process died; load the page again.
    Restart,
    /// The desktop changed shape.
    Resize(u32, u32),
    /// Load a page, replacing whatever is there.
    Load(String),
    /// Reload the current page, for the keybinding that does it.
    Reload,
    /// Input, on its way to the page. All plain numbers, which is what makes
    /// the crossing free: nothing here borrows anything of WebKit's.
    PointerMotion {
        time: u32,
        x: f64,
        y: f64,
        modifiers: u32,
    },
    PointerButton {
        time: u32,
        x: f64,
        y: f64,
        button: u32,
        pressed: bool,
        modifiers: u32,
    },
    PointerAxis {
        time: u32,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        precise: bool,
        modifiers: u32,
    },
    KeyboardKey {
        time: u32,
        keycode: u32,
        keysym: u32,
        pressed: bool,
        modifiers: u32,
    },
    /// Stop the loop and let the thread end.
    Quit,
}

/// A `GMainContext` pointer, sendable because the two calls made on it from
/// another thread — `wakeup` and `unref` — are the two GLib documents as
/// thread-safe.
struct Context(*mut c_void);

impl Drop for Context {
    fn drop(&mut self) {
        // The same contract that makes the pointer sendable makes this safe
        // from wherever the last handle lands: `g_main_context_unref`, like
        // `g_main_context_wakeup`, is documented thread-safe. Null is checked
        // because `start` builds one before it knows it is good, and a failed
        // creation drops it where it stands.
        if !self.0.is_null() {
            unsafe { g_main_context_unref(self.0) };
        }
    }
}

// SAFETY: see above. Every other use of this pointer is on the web thread.
unsafe impl Send for Context {}
unsafe impl Sync for Context {}

/// The queue the compositor posts into and the web thread drains.
struct Commands {
    queue: Mutex<VecDeque<Command>>,
    context: Context,
}

impl Commands {
    fn send(&self, command: Command) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.push_back(command);
        }
        // Wakes a `g_main_context_iteration` that is blocked in poll. This is
        // the whole reason no GSource is needed for the command channel:
        // `g_main_context_wakeup` is thread-safe by contract.
        unsafe { g_main_context_wakeup(self.context.0) };
    }
}

extern "C" {
    fn g_main_context_new() -> *mut c_void;
    fn g_main_context_push_thread_default(context: *mut c_void);
    fn g_main_context_pop_thread_default(context: *mut c_void);
    fn g_main_context_iteration(context: *mut c_void, may_block: i32) -> i32;
    fn g_main_context_wakeup(context: *mut c_void);
    fn g_main_context_unref(context: *mut c_void);
}

/// The shell, once it is running.
///
/// A handle. The engine itself is on the other side of [`Commands`].
pub struct Shell {
    pub mailbox: Arc<Mutex<Mailbox>>,
    commands: Arc<Commands>,
    /// Joined on drop — bounded, and detached if the thread will not come.
    /// See `Drop for Shell`.
    thread: Option<std::thread::JoinHandle<()>>,
}

struct Frames(Arc<Mutex<Mailbox>>);

impl FrameSink for Frames {
    fn frame(&mut self, frame: Frame, token: FrameToken) -> bool {
        // `try_lock`, not a blocking one, and left that way on purpose: a
        // refused frame is one WebKit repaints, while the messages and
        // termination below are state nobody else ever sends again.
        let Ok(mut mailbox) = self.0.try_lock() else {
            tracing::error!("the frame mailbox was already borrowed");
            return false;
        };

        let buffer = match to_dmabuf(&frame) {
            Ok(buffer) => buffer,
            Err(e) => {
                tracing::error!("could not describe the shell's frame: {e:#}");
                return false;
            }
        };

        // Replacing an undrawn frame hands its buffer back, which is what lets
        // WebKit carry on if the compositor is behind. Dropping the token
        // instead loses the buffer for good.
        if let Some(previous) = mailbox.frame.replace(Pending { buffer, token }) {
            tracing::trace!("dropped an undrawn shell frame");
            mailbox.stale.push(previous.token);
        }
        if let Some(ping) = mailbox.ping.as_ref() {
            ping.ping();
        }
        true
    }
}

struct Messages(Arc<Mutex<Mailbox>>);

impl MessageSink for Messages {
    fn message(&mut self, json: &str) {
        // Blocking, where the frames sink above refuses. This runs on the web
        // thread, and the compositor holds the mailbox only for the
        // microseconds a drain takes — but a message dropped here is a
        // `view.layout` the desktop never applies, so it waits rather than
        // vanish.
        if let Ok(mut mailbox) = self.0.lock() {
            mailbox.messages.push(json.to_owned());
            if let Some(ping) = mailbox.ping.as_ref() {
                ping.ping();
            }
        }
    }
}

struct Crashes(Arc<Mutex<Mailbox>>);

impl CrashSink for Crashes {
    fn terminated(&mut self, reason: Termination) {
        tracing::error!("the shell died: {reason}");
        // Blocking for the same reason as `Messages`: a termination dropped
        // here is a web process that is never recovered.
        if let Ok(mut mailbox) = self.0.lock() {
            // The frames in flight belonged to the process that just died.
            // Handing their tokens back would release buffers into a pool
            // that no longer exists, so they are dropped instead — `FrameToken`
            // is deliberately not `Drop`, which makes that a leak of a handle
            // whose owner is already gone rather than a call into freed memory.
            mailbox.frame = None;
            mailbox.stale.clear();
            mailbox.terminated = Some(reason);
            if let Some(ping) = mailbox.ping.as_ref() {
                ping.ping();
            }
        }
    }
}

// No `unsafe impl Send` for the three sinks any more: each holds nothing but
// an `Arc<Mutex<Mailbox>>`, and every field of `Mailbox` is `Send` on its own,
// so the compiler grants it. That matters beyond tidiness — the reason they
// were sound before was that everything stayed on one thread, and the shell is
// on its way to a thread of its own.

/// One page the in-process engine is drawing, and where it goes.
///
/// The same shape as `shell_client::ClientShell`, because the two backends
/// answer the same question differently and the compositor above them should
/// not have to care which: a list of pages, each with a rectangle in the output
/// layout, one of which runs the desktop. See `shell_client::plan_shells` for
/// what decides the list.
pub struct Page {
    pub engine: Shell,
    /// The document it was started on, so a restart loads the same thing and
    /// `sync_shells` can tell one page from another.
    pub url: String,
    /// Where in the output layout it lives.
    pub region: smithay::utils::Rectangle<i32, smithay::utils::Logical>,
    /// Whether this is the page that runs the desktop.
    pub desktop: bool,
    /// The size it was last told it is, so a layout change that does not alter
    /// it costs no repaint.
    pub size: Option<(u32, u32)>,
    /// The compositor's own copy of its newest frame, and that copy's size.
    ///
    /// A copy, unlike the out-of-process backend: WebKit paints into its own
    /// pool and reuses a buffer the moment it is released, so the frame has to
    /// be somewhere else before it is given back. See
    /// `ViewportState::import_shell_frame`.
    pub owned: Option<(
        smithay::backend::allocator::dmabuf::Dmabuf,
        smithay::utils::Size<i32, smithay::utils::Physical>,
    )>,
    /// What changed in that copy since the last frame. Required rather than an
    /// optimisation: an element that reports no damage tells the tracker
    /// nothing ever changes, and the output goes quiet after one frame.
    pub damage: smithay::backend::renderer::utils::DamageBag<i32, smithay::utils::Buffer>,
    /// Its render element id, stable for the life of the page.
    pub element_id: smithay::backend::renderer::element::Id,
    /// How many times this page's web process has died, and when the run
    /// began. Per page, because one page crashing says nothing about the other.
    pub restarts: u32,
    pub restart_window: Option<std::time::Instant>,
    /// Whether the shell has been told the desktop exists yet.
    pub announced: bool,
}

impl Page {
    /// A page in the layout's own coordinates, turned into one of its own.
    ///
    /// WebKit knows nothing about where its view sits on the desk: a click at
    /// the top-left of the second monitor is (0, 0) to the page drawn there.
    pub fn local(
        &self,
        at: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) -> smithay::utils::Point<f64, smithay::utils::Logical> {
        (
            at.x - self.region.loc.x as f64,
            at.y - self.region.loc.y as f64,
        )
            .into()
    }

    /// Whether a point in layout coordinates is on this page.
    pub fn contains(&self, at: smithay::utils::Point<f64, smithay::utils::Logical>) -> bool {
        self.region.to_f64().contains(at)
    }
}

/// How long the compositor waits for the web thread to report itself up.
///
/// Long enough that a cold WebKit on a slow disk, compiling shaders for a GPU
/// it has never seen, is not cut off; short enough that a hang is a message in
/// the log rather than a hung machine.
const STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long dropping the shell waits for the web thread to go.
///
/// Long enough that an engine tearing down cleanly is never cut off; short
/// enough that a thread wedged inside WebKit cannot hold up quitting. The
/// startup path refuses to join a wedged thread because waiting would inherit
/// the hang, and the same reasoning bounds this wait — with less at stake,
/// because the session is already on its way out.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How often a dropping shell looks to see whether its thread has gone. Short,
/// because the common case is a thread that exits in a few milliseconds.
const SHUTDOWN_POLL: std::time::Duration = std::time::Duration::from_millis(10);

impl Shell {
    /// Start the shell on `render_node`, showing `url`.
    ///
    /// `formats` is what WebKit may allocate — the renderer's own importable
    /// set, because a format the compositor cannot import produces a shell
    /// that never appears rather than an error.
    pub fn start(
        primary_node: &std::path::Path,
        render_node: &std::path::Path,
        formats: &[(u32, u64)],
        size: (u32, u32),
        url: &str,
        console: bool,
    ) -> Result<Self> {
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));

        // Made here and handed over, so the compositor has something to wake
        // before the thread has finished starting.
        let context = Context(unsafe { g_main_context_new() });
        anyhow::ensure!(!context.0.is_null(), "g_main_context_new returned NULL");
        let commands = Arc::new(Commands {
            queue: Mutex::new(VecDeque::new()),
            context,
        });

        // Owned by the thread, because every one of them is WebKit's.
        let primary_node = primary_node.to_owned();
        let render_node = render_node.to_owned();
        let formats = formats.to_vec();
        let url = url.to_owned();
        let (ready, started) = std::sync::mpsc::channel::<Result<(), String>>();

        let theirs = commands.clone();
        let post = mailbox.clone();
        let thread = std::thread::Builder::new()
            .name("viewport-shell".to_owned())
            .spawn(move || {
                web_thread(
                    theirs,
                    post,
                    primary_node,
                    render_node,
                    formats,
                    size,
                    url,
                    console,
                    ready,
                )
            })
            .map_err(|e| anyhow::anyhow!("spawning the shell thread: {e}"))?;

        // The engine has to be up before the compositor is told it is: a
        // failure here is "no shell", and it has to come back as an error
        // rather than as a desktop that never paints.
        //
        // Bounded, because this runs before the event loop does. A WebKit that
        // hangs here — a driver deadlock in the GPU process, an engine that
        // OOMs before it answers — is a compositor with no keyboard, no
        // control socket and no VT switch, on a machine whose only way out is
        // the power button. A slow start is worth waiting out; a start that
        // never finishes has to become an error while something can still be
        // done about it.
        match started.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                mailbox,
                commands,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(anyhow::anyhow!(e)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Queued rather than joined. The thread is wedged inside
                // WebKit by assumption, so waiting on it would inherit the
                // hang; the queue is drained as soon as its loop runs, so a
                // thread that does wake up later ends instead of painting into
                // a shell nobody is holding.
                commands.send(Command::Quit);
                Err(anyhow::anyhow!(
                    "the shell did not start within {}s",
                    STARTUP_TIMEOUT.as_secs()
                ))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(anyhow::anyhow!("the shell thread stopped before starting"))
            }
        }
    }

    /// Whether the web process has died since this was last asked.
    pub fn take_termination(&self) -> Option<Termination> {
        self.mailbox
            .try_lock()
            .map(|mut mailbox| mailbox.terminated.take())
            .unwrap_or(None)
    }

    /// Bring the shell back after its web process died.
    ///
    /// Loading rather than reloading: `reload` on a view whose process died
    /// during the initial load has nothing to reload, and this is exactly the
    /// case a shell that crashes on startup produces. WebKit spawns a fresh
    /// web process for the load, so the `WebKitWebView` itself — and every
    /// signal connected to it — survives.
    ///
    /// The URI is read on the web thread now. It has to be: asking a
    /// `WebKitWebView` anything from another thread is exactly the sort of
    /// call this split exists to prevent.
    pub fn restart(&self) -> Result<()> {
        self.commands.send(Command::Restart);
        Ok(())
    }

    /// Tell the shell how big the desktop is.
    pub fn resize(&self, width: u32, height: u32) {
        self.commands.send(Command::Resize(width, height));
    }

    /// Show a different page.
    pub fn load(&self, url: &str) {
        self.commands.send(Command::Load(url.to_owned()));
    }

    /// Reload what is showing.
    pub fn reload(&self) {
        self.commands.send(Command::Reload);
    }

    pub fn pointer_motion(&self, time: u32, x: f64, y: f64, modifiers: u32) {
        self.commands.send(Command::PointerMotion {
            time,
            x,
            y,
            modifiers,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn pointer_button(
        &self,
        time: u32,
        x: f64,
        y: f64,
        button: u32,
        pressed: bool,
        modifiers: u32,
    ) {
        self.commands.send(Command::PointerButton {
            time,
            x,
            y,
            button,
            pressed,
            modifiers,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn pointer_axis(
        &self,
        time: u32,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        precise: bool,
        modifiers: u32,
    ) {
        self.commands.send(Command::PointerAxis {
            time,
            x,
            y,
            dx,
            dy,
            precise,
            modifiers,
        });
    }

    pub fn keyboard_key(
        &self,
        time: u32,
        keycode: u32,
        keysym: u32,
        pressed: bool,
        modifiers: u32,
    ) {
        self.commands.send(Command::KeyboardKey {
            time,
            keycode,
            keysym,
            pressed,
            modifiers,
        });
    }

    /// Wake the event loop whenever the page posts something.
    pub fn wake_with(&self, ping: smithay::reexports::calloop::ping::Ping) {
        if let Ok(mut mailbox) = self.mailbox.try_lock() {
            mailbox.ping = Some(ping);
        }
    }

    /// Take everything the page has said since the last drain.
    pub fn take_messages(&self) -> Vec<String> {
        self.mailbox
            .try_lock()
            .map(|mut mailbox| std::mem::take(&mut mailbox.messages))
            .unwrap_or_default()
    }

    /// Take the newest painted frame, if there is one.
    pub fn take_frame(&self) -> Option<Pending> {
        self.mailbox
            .try_lock()
            .map(|mut mailbox| mailbox.frame.take())
            .unwrap_or(None)
    }

    /// Acknowledge a frame: it reached the screen, so WebKit's frame clock
    /// may schedule the next paint. The buffer stays on loan.
    pub fn frame_done(&self, token: &FrameToken) {
        // SAFETY: a second handle to a buffer the caller still owns, for a
        // message that only acknowledges it. The caller's token is what gets
        // released later; this one is dropped by the web thread.
        let token = unsafe { FrameToken::from_ptr(token.as_ptr()) };
        self.commands.send(Command::Done(token));
    }

    /// Give a frame's buffer back to WebKit's pool, once nothing samples it.
    pub fn frame_release(&self, token: FrameToken) {
        self.commands.send(Command::Release(token));
    }

    /// Frames that were superseded before anything drew them.
    pub fn take_stale(&self) -> Vec<FrameToken> {
        self.mailbox
            .try_lock()
            .map(|mut mailbox| std::mem::take(&mut mailbox.stale))
            .unwrap_or_default()
    }

    /// Send an event to the page.
    pub fn post(&self, event: &Event) -> Result<()> {
        let json = viewport_ipc::to_string(event)?;
        self.commands.send(Command::Post(json));
        Ok(())
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        // Asked to stop first: the thread owns WebKit, and returning from here
        // while it is still running would leave a web process attached to a
        // compositor that has gone.
        self.commands.send(Command::Quit);
        if let Some(thread) = self.thread.take() {
            // Bounded, like `stop_client_shells`: the startup path refuses to
            // join a thread that is wedged inside WebKit because the wait
            // would inherit the hang, and a drop must not do exactly that.
            let deadline = std::time::Instant::now() + SHUTDOWN_GRACE;
            while std::time::Instant::now() < deadline && !thread.is_finished() {
                std::thread::sleep(SHUTDOWN_POLL);
            }
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                // Detached rather than joined. It holds the other side of
                // `commands`, so the context and everything else it owns are
                // dropped when it does eventually wake — which for this
                // process may be never, and that is the price of quitting at
                // all behind an engine that will not stop.
                tracing::warn!(
                    "the shell's web thread did not stop within {}s; leaving it",
                    SHUTDOWN_GRACE.as_secs()
                );
            }
        }
    }
}

/// The web thread: WebKit, its context, and nothing else.
#[allow(clippy::too_many_arguments)]
fn web_thread(
    commands: Arc<Commands>,
    mailbox: Arc<Mutex<Mailbox>>,
    primary_node: std::path::PathBuf,
    render_node: std::path::PathBuf,
    formats: Vec<(u32, u64)>,
    size: (u32, u32),
    url: String,
    console: bool,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    let context = commands.context.0;
    // Before anything is created: WebKit attaches what it makes to the
    // thread-default context, and this is the thread that will iterate it.
    unsafe { g_main_context_push_thread_default(context) };

    let engine = (|| -> Result<(std::rc::Rc<Display>, WebView)> {
        let display = std::rc::Rc::new(Display::new(
            &primary_node,
            &render_node,
            &formats,
            Box::new(Frames(mailbox.clone())),
        )?);
        let view = WebView::new(
            display.clone(),
            Box::new(Messages(mailbox.clone())),
            Box::new(Crashes(mailbox.clone())),
            console,
        )?;
        // Size, map, focus, then load — the order the C build settled on.
        // Loading into an unmapped view of no size means the page runs and
        // never produces a frame.
        display.resize(size.0, size.1);
        display.show();
        view.load(&url)?;
        Ok((display, view))
    })();

    let (display, view) = match engine {
        Ok(engine) => {
            let _ = ready.send(Ok(()));
            engine
        }
        Err(e) => {
            let _ = ready.send(Err(format!("{e:#}")));
            unsafe { g_main_context_pop_thread_default(context) };
            return;
        }
    };

    loop {
        // Blocks until GLib has something, or until `Commands::send` wakes it.
        unsafe { g_main_context_iteration(context, 1) };

        let drained: Vec<Command> = match commands.queue.lock() {
            Ok(mut queue) => queue.drain(..).collect(),
            Err(_) => break,
        };
        for command in drained {
            match command {
                Command::Post(json) => {
                    if let Err(e) = view.post(&json) {
                        tracing::warn!("could not post to the shell: {e:#}");
                    }
                }
                Command::Done(token) => display.frame_done(&token),
                Command::Release(token) => view.frame_release(&token),
                Command::Resize(width, height) => display.resize(width, height),
                Command::Load(url) => {
                    if let Err(e) = view.load(&url) {
                        tracing::error!("could not load {url} in the shell: {e:#}");
                    }
                }
                Command::Reload => view.reload(),
                Command::PointerMotion {
                    time,
                    x,
                    y,
                    modifiers,
                } => display.pointer_motion(time, x, y, modifiers),
                Command::PointerButton {
                    time,
                    x,
                    y,
                    button,
                    pressed,
                    modifiers,
                } => display.pointer_button(time, x, y, button, pressed, modifiers),
                Command::PointerAxis {
                    time,
                    x,
                    y,
                    dx,
                    dy,
                    precise,
                    modifiers,
                } => display.pointer_axis(time, x, y, dx, dy, precise, modifiers),
                Command::KeyboardKey {
                    time,
                    keycode,
                    keysym,
                    pressed,
                    modifiers,
                } => display.keyboard_key(time, keycode, keysym, pressed, modifiers),
                Command::Restart => {
                    let url = view.uri().unwrap_or_else(|| url.clone());
                    if let Err(e) = view.load(&url) {
                        tracing::error!("could not restart the shell: {e:#}");
                    }
                    // The new process starts with a view of no size, exactly
                    // as a fresh one does, and paints nothing until told.
                    display.show();
                }
                Command::Quit => {
                    unsafe { g_main_context_pop_thread_default(context) };
                    return;
                }
            }
        }
    }
    unsafe { g_main_context_pop_thread_default(context) };
}

/// What to do about a web process that has just died.
///
/// The numbers are [`crate::shell_client::RESTART_LIMIT`] and
/// [`crate::shell_client::RESTART_WINDOW`], shared so the two backends cannot
/// drift apart; what a run earns when it is spent is where the policies part,
/// and deliberately — see `crate::shell_client::restart_backoff`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Restart, and this is which attempt of the current run it is.
    Restart(u32),
    /// The run has used up its budget. Retrying a page that cannot load is an
    /// infinite loop that spawns a process each time round.
    GiveUp(u32),
}

/// Decide whether a crash is worth restarting for, and update the run.
///
/// Split out from the compositor because the judgement is the whole point and
/// it is otherwise only reachable by crashing a real WebKit. A plain restart
/// limit is wrong in both directions: a desktop up for a week that has crashed
/// five times over that week is healthy, and one that crashes five times in
/// five seconds is a page that cannot load. The window is what tells them
/// apart — a crash far enough after the run began starts a new run.
pub fn budget(
    restarts: &mut u32,
    window: &mut Option<std::time::Instant>,
    now: std::time::Instant,
) -> Recovery {
    match *window {
        Some(began) if now.duration_since(began) < RESTART_WINDOW => *restarts += 1,
        _ => {
            *window = Some(now);
            *restarts = 1;
        }
    }

    if *restarts > RESTART_LIMIT {
        Recovery::GiveUp(*restarts)
    } else {
        Recovery::Restart(*restarts)
    }
}

/// Describe a WebKit frame as a Smithay `Dmabuf`.
fn to_dmabuf(frame: &Frame) -> Result<Dmabuf> {
    let code = Fourcc::try_from(frame.format)
        .map_err(|_| anyhow::anyhow!("unknown fourcc {:#x}", frame.format))?;

    let mut builder = Dmabuf::builder(
        (frame.width as i32, frame.height as i32),
        code,
        Modifier::from(frame.modifier),
        DmabufFlags::empty(),
    );
    for plane in &frame.planes {
        // The fds were already duplicated when the frame crossed the FFI
        // boundary, so this hands over ownership rather than borrowing again.
        let fd = plane.fd.try_clone()?;
        if !builder.add_plane(fd, plane.offset, plane.stride) {
            anyhow::bail!("too many planes for a dmabuf");
        }
    }
    builder
        .build()
        .ok_or_else(|| anyhow::anyhow!("the frame did not describe a complete buffer"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Drive `budget` over a list of offsets from a fixed start.
    fn run(offsets: &[Duration]) -> Vec<Recovery> {
        let start = Instant::now();
        let mut restarts = 0;
        let mut window = None;
        offsets
            .iter()
            .map(|offset| budget(&mut restarts, &mut window, start + *offset))
            .collect()
    }

    #[test]
    fn a_first_crash_is_restarted() {
        assert_eq!(run(&[Duration::ZERO]), vec![Recovery::Restart(1)]);
    }

    #[test]
    fn a_page_that_cannot_load_is_given_up_on() {
        // A shell that throws during startup crashes as fast as it can be
        // spawned. Without the limit that is an infinite loop that forks.
        let burst: Vec<_> = (0..RESTART_LIMIT + 2)
            .map(|n| Duration::from_millis(u64::from(n) * 100))
            .collect();
        let outcome = run(&burst);
        assert_eq!(
            outcome[..RESTART_LIMIT as usize],
            (1..=RESTART_LIMIT)
                .map(Recovery::Restart)
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            outcome[RESTART_LIMIT as usize],
            Recovery::GiveUp(_)
        ));
    }

    #[test]
    fn crashes_spread_out_never_use_up_the_budget() {
        // A desktop up for a week that has crashed once a day is healthy. A
        // plain counter would give up on it on the sixth day.
        let spread: Vec<_> = (0..20)
            .map(|n| RESTART_WINDOW * (n + 1) + Duration::from_secs(1))
            .collect();
        assert!(run(&spread)
            .iter()
            .all(|outcome| *outcome == Recovery::Restart(1)));
    }

    #[test]
    fn a_quiet_spell_starts_the_run_over() {
        // Four crashes, then quiet, then four more: eight in total, which a
        // plain counter would have given up on.
        let mut offsets: Vec<_> = (0..4).map(Duration::from_secs).collect();
        offsets.extend((0..4).map(|n| RESTART_WINDOW + Duration::from_secs(n + 10)));
        let outcome = run(&offsets);
        assert_eq!(
            outcome,
            vec![
                Recovery::Restart(1),
                Recovery::Restart(2),
                Recovery::Restart(3),
                Recovery::Restart(4),
                Recovery::Restart(1),
                Recovery::Restart(2),
                Recovery::Restart(3),
                Recovery::Restart(4),
            ]
        );
    }

    #[test]
    fn a_termination_asked_for_is_not_a_crash() {
        // Something wanted the process gone; restarting would fight it.
        assert!(!Termination::TerminatedByApi.is_recoverable());
        assert!(Termination::Crashed.is_recoverable());
        assert!(Termination::ExceededMemoryLimit.is_recoverable());
    }
}
