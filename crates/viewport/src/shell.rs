// SPDX-License-Identifier: GPL-3.0-or-later
//
// The shell: WebKit painting the desktop. Ports src/web.c.
//
// The compositor never touches these pixels. WebKit paints into a DMA-BUF on
// the same GPU the renderer uses, hands it over, and the buffer is imported as
// a Vulkan image — the path `viewport_vulkan::Image::import` already takes for
// client buffers, because a frame from WebKit is not special.
//
// Frames arrive on the GLib thread, which is the compositor's thread, so
// nothing here is shared across threads. What it cannot do is reach into the
// compositor directly: the frame callback runs inside WebKit, underneath a
// calloop dispatch that already holds `&mut ViewportState`. So frames land in
// a queue and are picked up at the top of the next render.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::{Fourcc, Modifier};

use viewport_ipc::Event;
use viewport_web::webkit::{CrashSink, MessageSink, Termination, WebView};
use viewport_web::wpe::{Display, FrameSink, FrameToken};
use viewport_web::Frame;

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

/// The shell, once it is running.
pub struct Shell {
    pub view: WebView,
    pub display: std::rc::Rc<Display>,
    pub mailbox: Arc<Mutex<Mailbox>>,
    /// What the shell was started from, kept for [`Shell::restart`]: a web
    /// process that died during the initial load leaves the view with no URI
    /// of its own to reload.
    url: String,
}

struct Frames(Arc<Mutex<Mailbox>>);

impl FrameSink for Frames {
    fn frame(&mut self, frame: Frame, token: FrameToken) -> bool {
        let Ok(mut mailbox) = self.0.try_lock() else {
            // Re-entered, which should not happen on one thread. Refusing is
            // better than blocking WebKit forever.
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
        if let Ok(mut mailbox) = self.0.try_lock() {
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
        if let Ok(mut mailbox) = self.0.try_lock() {
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

        let display = std::rc::Rc::new(Display::new(
            primary_node,
            render_node,
            formats,
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
        view.load(url)?;

        Ok(Self {
            view,
            display,
            mailbox,
            url: url.to_owned(),
        })
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
    pub fn restart(&self) -> Result<()> {
        let url = self.view.uri().unwrap_or_else(|| self.url.clone());
        self.view.load(&url)?;
        // The new process starts with a view of no size, exactly as a fresh
        // one does, and paints nothing until told otherwise.
        self.display.show();
        Ok(())
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
        self.display.frame_done(token);
    }

    /// Give a frame's buffer back to WebKit's pool, once nothing samples it.
    pub fn frame_release(&self, token: FrameToken) {
        self.view.frame_release(&token);
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
        self.view.post(&json)
    }
}

/// How many restarts inside [`RESTART_WINDOW`] before giving up.
pub const RESTART_LIMIT: u32 = 5;

/// A crash this long after a run of them started begins a new run.
pub const RESTART_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

/// What to do about a web process that has just died.
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
