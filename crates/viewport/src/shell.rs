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

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::{Fourcc, Modifier};

use viewport_ipc::Event;
use viewport_web::wpe::{Display, FrameSink, FrameToken};
use viewport_web::webkit::{MessageSink, WebView};
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
}

/// The shell, once it is running.
pub struct Shell {
    pub view: WebView,
    pub display: std::sync::Arc<Display>,
    pub mailbox: Rc<RefCell<Mailbox>>,
}

struct Frames(Rc<RefCell<Mailbox>>);

impl FrameSink for Frames {
    fn frame(&mut self, frame: Frame, token: FrameToken) -> bool {
        let Ok(mut mailbox) = self.0.try_borrow_mut() else {
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

struct Messages(Rc<RefCell<Mailbox>>);

impl MessageSink for Messages {
    fn message(&mut self, json: &str) {
        if let Ok(mut mailbox) = self.0.try_borrow_mut() {
            mailbox.messages.push(json.to_owned());
            if let Some(ping) = mailbox.ping.as_ref() {
                ping.ping();
            }
        }
    }
}

// SAFETY: both are only ever used on the thread that drives GLib, which is the
// compositor's thread. The Send bound on the sink traits exists for callers
// that do move them between threads; this one does not.
unsafe impl Send for Frames {}
unsafe impl Send for Messages {}

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
        let mailbox = Rc::new(RefCell::new(Mailbox::default()));

        let display = std::sync::Arc::new(Display::new(
            primary_node,
            render_node,
            formats,
            Box::new(Frames(mailbox.clone())),
        )?);

        let view = WebView::new(
            display.clone(),
            Box::new(Messages(mailbox.clone())),
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
        })
    }

    /// Wake the event loop whenever the page posts something.
    pub fn wake_with(&self, ping: smithay::reexports::calloop::ping::Ping) {
        if let Ok(mut mailbox) = self.mailbox.try_borrow_mut() {
            mailbox.ping = Some(ping);
        }
    }

    /// Take everything the page has said since the last drain.
    pub fn take_messages(&self) -> Vec<String> {
        self.mailbox
            .try_borrow_mut()
            .map(|mut mailbox| std::mem::take(&mut mailbox.messages))
            .unwrap_or_default()
    }

    /// Take the newest painted frame, if there is one.
    pub fn take_frame(&self) -> Option<Pending> {
        self.mailbox
            .try_borrow_mut()
            .and_then(|mut mailbox| Ok(mailbox.frame.take()))
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
            .try_borrow_mut()
            .map(|mut mailbox| std::mem::take(&mut mailbox.stale))
            .unwrap_or_default()
    }

    /// Send an event to the page.
    pub fn post(&self, event: &Event) -> Result<()> {
        let json = viewport_ipc::to_string(event)?;
        self.view.post(&json)
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
