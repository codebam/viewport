// SPDX-License-Identifier: GPL-3.0-or-later
//
// The PipeWire half of screen sharing.
//
// Everything that consumes a screen share — a browser, OBS, a recorder —
// receives it as a PipeWire stream, so a compositor that wants to offer
// windows as well as monitors has to publish one itself. That is the whole
// reason this exists: xdg-desktop-portal-wlr can only offer monitors, because
// wlr-screencopy can only capture outputs, and a portal that owns its own
// transport can offer whatever the compositor can composite.
//
// The loop runs inside the compositor's, not on a thread of its own. Frames
// are produced where the renderer is, and handing buffers between threads to
// avoid a few lines of event-loop plumbing would mean synchronising them
// afterwards anyway.

use std::collections::HashMap;
use std::io::Cursor;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pipewire as pw;
use pw::spa;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::Buffer as _;
use smithay::utils::{Physical, Size};

/// How many buffers a stream cycles through.
///
/// Enough that the compositor can draw one while the consumer reads another,
/// few enough that a stalled consumer does not pin much: each is a whole
/// screen.
pub const BUFFERS: usize = 3;

/// What PipeWire calls a stream that has not been given a node yet.
const INVALID_NODE: u32 = u32::MAX;

/// Where stream identities come from.
///
/// A counter rather than anything derived from the stream, because the point
/// is to tell two half-made streams apart before either has a node — and
/// until the daemon answers, every one of them looks like every other.
static NEXT_STREAM: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_stream_id() -> u64 {
    NEXT_STREAM.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// What the stream has agreed to produce, which a renegotiation replaces.
///
/// Shared rather than captured: the callbacks are registered once and live as
/// long as the stream, and the whole point of renegotiating is that what they
/// have to answer with is not what it was when they were written.
#[derive(Clone, Copy, Debug)]
struct Agreed {
    size: Size<i32, Physical>,
    layout: Option<Layout>,
}

/// What the buffers the compositor allocated look like.
///
/// Taken from a real allocation rather than computed: the driver chooses the
/// modifier and the stride, and a consumer told anything else cannot import
/// what it is sent.
#[derive(Clone, Copy, Debug)]
struct Layout {
    modifier: u64,
    stride: i32,
    offset: u32,
    size: u32,
}

impl Layout {
    fn of(target: &Dmabuf) -> Option<Self> {
        let stride = target.strides().next()? as i32;
        let offset = target.offsets().next()?;
        Some(Self {
            modifier: u64::from(target.format().modifier),
            stride,
            offset,
            size: stride as u32 * target.height(),
        })
    }
}

/// Memory the compositor handed to PipeWire for one buffer.
///
/// Held for as long as PipeWire has the buffer: the descriptor is the
/// compositor's, and closing it while the consumer is still importing from it
/// is a picture that stops.
enum Memory {
    /// A buffer the GPU draws straight into.
    Dma(Dmabuf),
    /// Shared memory the compositor writes with the CPU, for a consumer that
    /// cannot import a DMA-BUF.
    Shared {
        _fd: OwnedFd,
        /// As an integer, so the map can be shared with PipeWire's thread — a
        /// raw pointer is not Send and this is only ever unmapped here.
        ptr: usize,
        len: usize,
    },
}

impl Drop for Memory {
    fn drop(&mut self) {
        if let Memory::Shared { ptr, len, .. } = self {
            // SAFETY: this mapping was made below and is unmapped once, when
            // PipeWire has given the buffer back.
            unsafe {
                let _ = smithay::reexports::rustix::mm::munmap(*ptr as *mut _, *len);
            }
        }
    }
}

/// A stream a client is watching.
pub struct Stream {
    pub stream: pw::stream::StreamRc,
    /// Kept alive: dropping the listener stops the callbacks.
    _listener: pw::stream::StreamListener<()>,
    pub size: Size<i32, Physical>,
    /// Whether a consumer is actually reading.
    ///
    /// Compositing and reading back a screen costs a full frame off the GPU
    /// every time — fifteen megabytes at 1440p — and doing it for a stream in
    /// any other state is that cost for nothing. A session that has been
    /// created and not started, or one whose consumer has gone away, leaves
    /// the stream paused, and the compositor was paying for it anyway.
    streaming: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// When the last frame went out, so a share does not ask the renderer for
    /// more than it can use.
    last: Option<std::time::Instant>,
    /// The node a client connects to, which is what the portal hands back
    /// over D-Bus. `u32::MAX` until the daemon names the stream, which it
    /// does on its own clock; anything that needs the truth asks
    /// [`Stream::arrival`] rather than reading this. The comparisons made
    /// against it — matching a `Close` to its stream — are only ever made
    /// with real node numbers, so the placeholder never matches by accident.
    pub node_id: u32,
    /// An identity for the stream while it is the only one of its kind.
    ///
    /// The node cannot stand in: until it arrives, every unfinished stream
    /// answers the same 0xffffffff, and taking the wrong one back out would
    /// be a teardown of somebody else's share. Minted here so a stream and
    /// the record of the reply it owes are recognisably a pair from birth.
    pub id: u64,
    /// What became of the node, shared with whoever answers for the stream.
    arrival: Arrival,
    /// Kept until the answer is in: dropping the listener takes its hook
    /// back out, and it must not go early or the answer could be missed.
    _done_listener: pw::core::Listener,
    /// What the callbacks answer with, which changes when the source resizes.
    agreed: Arc<Mutex<Agreed>>,
    /// The buffers waiting to be handed out, refilled on a renegotiation.
    pool: Arc<Mutex<Vec<Dmabuf>>>,
    /// When the format was last renegotiated, so dragging a window's edge does
    /// not allocate three screens' worth of buffers per frame of the drag.
    renegotiated: Option<std::time::Instant>,
    /// Whether the format that was agreed is one the GPU can draw into.
    ///
    /// Decided by the consumer: both are offered, and one that cannot import a
    /// DMA-BUF picks the shared-memory format instead.
    dmabuf: Arc<AtomicBool>,
    /// What was handed over for each buffer, by the descriptor it went out
    /// under. The descriptor is how a buffer coming back is recognised —
    /// PipeWire hands back the same `spa_data`, and nothing else in it is ours.
    memory: Arc<Mutex<HashMap<i64, Memory>>>,
}

impl Stream {
    /// The story of this stream being named, to wait on or answer from.
    pub fn arrival(&self) -> Arrival {
        self.arrival.clone()
    }

    /// Hand one frame to whoever is watching.
    ///
    /// A frame is dropped rather than queued when the consumer has not
    /// returned a buffer: a screen share that falls behind should show the
    /// newest frame late, not every frame later still.
    /// Whether it is worth compositing a frame for this stream at all.
    pub fn wants_frame(&self, rate: std::time::Duration) -> bool {
        if !self.streaming.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        self.last.map(|at| at.elapsed() >= rate).unwrap_or(true)
    }

    /// How long the source has to hold still before the format is agreed
    /// again.
    ///
    /// Renegotiating allocates three screens' worth of buffers and costs a
    /// round trip with the consumer, and dragging a window's edge produces a
    /// new size every frame. Frames are dropped in the meantime, which is what
    /// was happening for the whole of a share before any of this existed.
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

    /// Whether the source has changed size out from under the agreed format.
    ///
    /// A stream whose format says one size and whose frames arrive at another
    /// drops every one of them: 488 of them in one run, with nothing but a
    /// debug line to say the share had silently frozen on its last good frame.
    pub fn needs_renegotiation(&self, size: Size<i32, Physical>) -> bool {
        // Not for a stream nobody is reading. A share whose consumer has gone
        // away — a closed tab whose session the frontend has not got round to
        // closing — would otherwise allocate three screens' worth of buffers
        // every time the window behind it was resized, for a picture no one
        // will see. It is renegotiated when it resumes, which is the first
        // frame anybody asks for.
        if !self.streaming.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        needs_renegotiation(*self.agreed.lock().unwrap(), self.renegotiated, size)
    }

    /// Agree a new format, at the size the source is now.
    ///
    /// In place, rather than by making a new stream: the consumer is connected
    /// to a node number it was given once, and a stream that went away and came
    /// back under a new number would be a share that stops.
    pub fn renegotiate(
        &mut self,
        size: Size<i32, Physical>,
        targets: Vec<Dmabuf>,
        loop_: &pw::thread_loop::ThreadLoop,
    ) -> anyhow::Result<()> {
        let _guard = loop_.lock();

        let agreed = Agreed {
            size,
            layout: targets.first().and_then(Layout::of),
        };
        // Before the offer goes out: PipeWire may ask for buffers as soon as
        // the format is settled, and it asks on its own thread.
        *self.pool.lock().unwrap() = targets;
        *self.agreed.lock().unwrap() = agreed;

        let described = offered_formats(agreed)?;
        let mut params = described
            .iter()
            .map(|bytes| {
                spa::pod::Pod::from_bytes(bytes)
                    .ok_or_else(|| anyhow::anyhow!("the format description is not a valid pod"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.stream
            .update_params(&mut params)
            .map_err(|e| anyhow::anyhow!("offering a new format: {e}"))?;

        self.size = size;
        self.renegotiated = Some(std::time::Instant::now());
        tracing::info!("screencast: the source is {}x{} now", size.w, size.h);
        Ok(())
    }

    /// Whether frames for this stream are drawn by the GPU rather than copied.
    pub fn uses_dmabuf(&self) -> bool {
        self.dmabuf.load(Ordering::Relaxed)
    }

    /// Draw one frame straight into the buffer the consumer will read.
    ///
    /// No readback and no copy: `fill` is handed the DMA-BUF that is about to
    /// go out, and composites into it. That is the whole difference between
    /// this and `push` — the shared-memory path pulls a whole screen back
    /// across the bus and then writes it out again, thirty times a second.
    pub fn with_target<F>(
        &mut self,
        size: Size<i32, Physical>,
        loop_: &pw::thread_loop::ThreadLoop,
        fill: F,
    ) where
        F: FnOnce(&Dmabuf) -> Result<(), String>,
    {
        self.last = Some(std::time::Instant::now());
        let _guard = loop_.lock();
        if size != self.size {
            tracing::debug!(
                "screencast: a {}x{} frame for a {}x{} stream",
                size.w,
                size.h,
                self.size.w,
                self.size.h
            );
            return;
        }
        let Some(mut buffer) = self.stream.dequeue_buffer() else {
            tracing::debug!("screencast: no buffer to fill");
            return;
        };

        let Some(data) = buffer.datas_mut().first_mut() else {
            tracing::warn!("screencast: a buffer with nothing in it");
            return;
        };
        // Which of ours this is. The descriptor is the only part of the buffer
        // that came from this end, so it is what identifies it.
        let fd = data.as_raw().fd;
        let held = self.memory.lock().unwrap();
        let Some(Memory::Dma(target)) = held.get(&fd) else {
            tracing::warn!("screencast: a buffer on descriptor {fd} that was never handed out");
            return;
        };
        let Some(layout) = Layout::of(target) else {
            tracing::warn!("screencast: a target with no planes");
            return;
        };

        tracing::debug!("screencast: drawing into a buffer");
        if let Err(e) = fill(target) {
            tracing::warn!("screencast: {e}");
            return;
        }

        // What was drawn. A consumer reads the chunk rather than the buffer,
        // and one left at zero is a frame of nothing.
        let chunk = data.chunk_mut();
        *chunk.size_mut() = layout.size;
        *chunk.stride_mut() = layout.stride;
        *chunk.offset_mut() = layout.offset;
    }

    pub fn push(
        &mut self,
        pixels: &[u8],
        size: Size<i32, Physical>,
        loop_: &pw::thread_loop::ThreadLoop,
    ) {
        self.last = Some(std::time::Instant::now());
        // The loop's thread is dispatching this stream; touching its buffers
        // without the lock races with it.
        let _guard = loop_.lock();
        if size != self.size {
            // The output changed mode, or a nested window was resized. The
            // format was negotiated for the old size, so this frame is not
            // what the consumer agreed to read — and silently dropping every
            // one of them is a stream that plays and delivers nothing, so it
            // is worth saying.
            tracing::debug!(
                "screencast: a {}x{} frame for a {}x{} stream",
                size.w,
                size.h,
                self.size.w,
                self.size.h
            );
            return;
        }
        let Some(mut buffer) = self.stream.dequeue_buffer() else {
            tracing::debug!("screencast: no buffer to fill");
            return;
        };
        tracing::debug!("screencast: filling a buffer");
        let stride = size.w * 4;
        let wanted = (stride * size.h) as usize;

        {
            let Some(data) = buffer.datas_mut().first_mut() else {
                tracing::warn!("screencast: a buffer with nothing in it");
                return;
            };
            let Some(destination) = data.data() else {
                // A buffer the compositor cannot write into — a DMA-BUF, or
                // one PipeWire did not map. Returning here leaves the chunk at
                // zero, which a consumer reads as a frame of nothing: it
                // connects, streams, and shows an empty picture.
                tracing::warn!("screencast: a buffer that cannot be written to");
                return;
            };
            if destination.len() < wanted || pixels.len() < wanted {
                tracing::warn!(
                    "screencast: {} bytes of frame and {} of buffer, for {wanted} wanted",
                    pixels.len(),
                    destination.len()
                );
                return;
            }
            destination[..wanted].copy_from_slice(&pixels[..wanted]);

            // What was actually written. A consumer reads the chunk rather
            // than the buffer, and one left at zero is a frame of nothing.
            let chunk = data.chunk_mut();
            *chunk.size_mut() = wanted as u32;
            *chunk.stride_mut() = stride;
            *chunk.offset_mut() = 0;
        }

        // Handed back by dropping it, which is what the binding does for us.
        drop(buffer);
    }
}

/// How long the daemon gets to name a node before the share is refused.
///
/// A ceiling rather than a delay: an ordinary share arrives in single
/// milliseconds, and the ceiling is only reached when PipeWire is not going
/// to answer at all. Enforced from the compositor's timers rather than in
/// here, because giving up means tearing a half-made stream back out of the
/// compositor, and that wants the compositor's thread — see
/// [`Arrival::when_named`] for why the failure cannot be reported from the
/// thread the answer arrives on.
pub const NODE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Whether a stream has been given its node yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrived {
    /// The daemon has not answered yet.
    Yet,
    /// It has, and this is the node.
    Now(u32),
    /// Nothing is coming: the deadline passed, or the daemon answered
    /// without a node, which is the same refusal in different clothes.
    Never,
}

/// The one-way story of a stream being named, told from both sides.
///
/// The daemon's answer is dispatched on the thread loop's thread; whoever
/// answers the portal for this stream waits on the same little cell, and
/// [`when_named`](Arrival::when_named) is how the answer reaches them. The
/// success crosses threads freely — it is a number and a channel send — so
/// it leaves from wherever the news breaks. The failure does not: refusing a
/// share means tearing a half-made stream out of the compositor's hands, and
/// that wants `&mut` on the compositor, so it can only ever be reported by
/// the deadline armed against [`NODE_TIMEOUT`], and here it is only recorded.
#[derive(Clone)]
pub struct Arrival {
    inner: Arc<std::sync::Mutex<ArrivalInner>>,
}

/// What one side fills in and the other reads.
///
/// One lock rather than two so that whether an answer is still owed cannot
/// depend on the order two threads happened to take their locks in: the
/// `done` event, the promise made while it was in flight and the deadline
/// claiming the refusal all meet here, and exactly one of them gets to say
/// anything.
struct ArrivalInner {
    /// `None` until the round trip completes; `Some` carries whatever the
    /// node id says at that point. A claim by the deadline writes the
    /// invalid node into it, which closes the cell behind itself: a `done`
    /// event landing afterwards finds it answered and stays quiet.
    node: Option<u32>,
    /// Promises to answer with the node, taken and fired the moment it
    /// exists. Usually one, always drained together — a second answer must
    /// not reach anybody however it arrives.
    waiting: Vec<Box<dyn FnOnce(u32) + Send>>,
}

impl Arrival {
    fn new() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(ArrivalInner {
                node: None,
                waiting: Vec::new(),
            })),
        }
    }

    /// Promise to answer with the node, the moment there is one.
    ///
    /// Kept on whichever thread the news arrives on — the thread loop's,
    /// normally, with the loop held — so what is promised must not call back
    /// into it. Sending the portal's reply is a channel write, which touches
    /// nothing of PipeWire's; that is exactly why the success can leave from
    /// here at all.
    ///
    /// Made after the stream exists but possibly before it is named, and
    /// once per stream: a promise made of an answer already in hand is kept
    /// on the spot, and one made of a failed share is quietly dropped, since
    /// the deadline owes that refusal and has already claimed it.
    pub fn when_named(&self, then: impl FnOnce(u32) + Send + 'static) {
        let node = {
            let mut inner = self.inner.lock().unwrap();
            match inner.node {
                Some(node) if node != INVALID_NODE => node,
                // Answered badly: the refusal belongs to the deadline.
                Some(_) => return,
                None => {
                    inner.waiting.push(Box::new(then));
                    return;
                }
            }
        };
        // Kept outside the lock, on principle: whoever asked may answer back
        // into this arrival, and a callback run holding the cell would turn
        // that into a deadlock.
        then(node)
    }

    /// How things stand.
    pub fn status(&self) -> Arrived {
        match self.inner.lock().unwrap().node {
            None => Arrived::Yet,
            Some(INVALID_NODE) => Arrived::Never,
            Some(node) => Arrived::Now(node),
        }
    }

    /// Take the refusal on, unless the stream has already succeeded.
    ///
    /// True when the Ok reply is still owed and nobody else will ever send
    /// it — the deadline's cue to refuse and tear the stream down. False when
    /// the node arrived first and the answer is already gone. Claiming marks
    /// the arrival failed as it goes, which is what keeps a `done` event
    /// arriving late from answering anybody twice.
    pub(crate) fn fail(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner.node {
            Some(node) if node != INVALID_NODE => false,
            _ => {
                inner.node = Some(INVALID_NODE);
                true
            }
        }
    }

    /// The daemon's answer, from the `done` listener.
    ///
    /// Answers once. A second `done` for the same round trip cannot happen,
    /// and neither can a first one after the deadline gave up: both find the
    /// cell filled and the audience gone.
    fn announced(&self, node: u32) {
        let mut inner = self.inner.lock().unwrap();
        if inner.node.is_some() {
            return;
        }
        inner.node = Some(node);
        let waiting = std::mem::take(&mut inner.waiting);
        drop(inner);
        if node != INVALID_NODE {
            for then in waiting {
                then(node);
            }
        }
        // A daemon that answered without a node has refused the share in its
        // own way. The promises were success-only and die here; saying so to
        // the application falls to the deadline, which is where the teardown
        // lives anyway.
    }
}

/// The compositor's connection to PipeWire.
///
/// On a thread of its own rather than inside the compositor's event loop.
/// Attaching PipeWire's loop to calloop stopped the compositor dispatching
/// anything at all — no redraws, no input, no clients — from the moment a
/// stream was created, and a screen share is not worth a frozen desktop. The
/// thread loop is what xdg-desktop-portal-wlr uses for the same reason: every
/// call into it takes the loop's own lock, so frames can be handed over from
/// wherever the renderer happens to be.
/// The `Rc` variants throughout: pipewire-rs 0.9 splits every object into an
/// owning `Box` form and a reference-counted `Rc` one, and a stream has to hold
/// the core alive, so the core has to be shared rather than owned here.
pub struct Pipewire {
    pub thread_loop: pw::thread_loop::ThreadLoopRc,
    pub core: pw::core::CoreRc,
    /// Held because dropping it tears the connection down.
    _context: pw::context::ContextRc,
}

impl Pipewire {
    /// Connect, or say why not.
    ///
    /// A session with no PipeWire is a session with no screen sharing and a
    /// working desktop otherwise, so this is reported rather than fatal.
    pub fn new() -> anyhow::Result<Self> {
        pw::init();
        // SAFETY: the loop is created before it is started, and every call
        // into it below takes its lock — which is the contract the binding
        // marks unsafe for.
        let thread_loop =
            unsafe { pw::thread_loop::ThreadLoopRc::new(Some("viewport-screencast"), None) }
                .map_err(|e| anyhow::anyhow!("creating a pipewire loop: {e}"))?;

        // Everything below runs with the loop held, which is the contract:
        // the thread is already dispatching, and touching its objects without
        // the lock is a race with it.
        let (context, core) = {
            let _guard = thread_loop.lock();
            let context = pw::context::ContextRc::new(&thread_loop, None)
                .map_err(|e| anyhow::anyhow!("creating a pipewire context: {e}"))?;
            let core = context
                .connect_rc(None)
                .map_err(|e| anyhow::anyhow!("connecting to pipewire: {e}"))?;
            (context, core)
        };
        thread_loop.start();

        Ok(Self {
            thread_loop,
            core,
            _context: context,
        })
    }

    /// Publish a stream of `size`, in the format a screen share is expected to
    /// arrive in.
    ///
    /// BGRx rather than anything with alpha: what is captured is a screen,
    /// which is opaque, and a consumer that takes the fourth byte for alpha
    /// shows a transparent picture.
    ///
    /// Returns before the daemon has named it: what the stream is called is
    /// on [`Stream::arrival`], and whoever answers the portal for it waits
    /// there rather than here. Nothing in this function waits at all — a
    /// share used to stop the desktop dead for as long as that took.
    pub fn create_stream(
        &self,
        name: &str,
        size: Size<i32, Physical>,
        targets: Vec<Dmabuf>,
    ) -> anyhow::Result<Stream> {
        // Everything below runs with the loop held. It has to: the stream is
        // registered, connected and given its round trip here, all against
        // objects the thread loop's thread is dispatching on its own.
        let _guard = self.thread_loop.lock();
        let stream = pw::stream::StreamRc::new(
            self.core.clone(),
            name,
            pw::properties::properties! {
                *pw::keys::MEDIA_CLASS => "Video/Source",
                *pw::keys::MEDIA_ROLE => "Screen",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::NODE_NAME => name,
            },
        )
        .map_err(|e| anyhow::anyhow!("creating a pipewire stream: {e}"))?;

        // What the compositor is going to hand over, if it managed to
        // allocate anything. A backend with no GPU allocator — nested, or
        // headless — has none, and offers only the shared-memory format.
        let agreed = Arc::new(Mutex::new(Agreed {
            size,
            layout: targets.first().and_then(Layout::of),
        }));
        let pool = Arc::new(Mutex::new(targets));
        let memory: Arc<Mutex<HashMap<i64, Memory>>> = Arc::new(Mutex::new(HashMap::new()));
        let dmabuf = Arc::new(AtomicBool::new(false));

        let streaming = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = streaming.clone();
        let chose_dmabuf = dmabuf.clone();
        let added = memory.clone();
        let removed = memory.clone();
        let negotiating = agreed.clone();
        let allocating = agreed.clone();
        let handing_out = pool.clone();
        let listener = stream
            .add_local_listener_with_user_data(())
            .state_changed(move |_stream, (), old, new| {
                tracing::debug!("screencast stream: {old:?} -> {new:?}");
                flag.store(
                    matches!(new, pw::stream::StreamState::Streaming),
                    std::sync::atomic::Ordering::Relaxed,
                );
            })
            // What the consumer needs to allocate against.
            //
            // A stream that never answers this never starts. gstreamer
            // tolerated its absence and Firefox did not: the portal handed
            // back a node, the browser connected to nothing, and the log
            // showed a stream that reached Paused and stopped there.
            .param_changed(move |stream, (), id, pod| {
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Some(pod) = pod else { return };
                let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(pod)
                else {
                    return;
                };
                if media_type != spa::param::format::MediaType::Video
                    || media_subtype != spa::param::format::MediaSubtype::Raw
                {
                    return;
                }

                // Which of the two offers was taken. A format carrying a
                // modifier is a DMA-BUF one — nothing else needs to describe
                // how the pixels are laid out in memory the consumer imports.
                let agreed = *negotiating.lock().unwrap();
                let size = agreed.size;
                let chosen = agreed.layout.filter(|_| carries_modifier(pod));
                chose_dmabuf.store(chosen.is_some(), Ordering::Relaxed);
                tracing::debug!(
                    "screencast: sharing through {}",
                    if chosen.is_some() {
                        "a dma-buf"
                    } else {
                        "shared memory"
                    }
                );

                match buffer_params(size, chosen) {
                    Ok(params) => {
                        let Some(pod) = spa::pod::Pod::from_bytes(&params) else {
                            tracing::warn!("screencast: the buffer parameters are not a pod");
                            return;
                        };
                        if let Err(e) = stream.update_params(&mut [pod]) {
                            tracing::warn!("screencast: buffer parameters refused: {e}");
                        } else {
                            tracing::debug!("screencast: buffer parameters published");
                        }
                    }
                    Err(e) => tracing::warn!("screencast: {e}"),
                }
            })
            // The memory for one buffer.
            //
            // This end allocates, which is what ALLOC_BUFFERS means: PipeWire
            // has no GPU allocator and cannot make a buffer the compositor can
            // render into, so it asks. Answering with nothing is a stream that
            // negotiates, plays, and delivers frames with maxsize zero — a
            // consumer sees an empty picture and no log says why.
            .add_buffer(move |_stream, (), raw| {
                // SAFETY: PipeWire hands over a buffer it owns for exactly as
                // long as this callback, and calls it on its own thread with
                // the loop held.
                unsafe {
                    let Some(data) = first_data(raw) else {
                        tracing::warn!("screencast: a buffer with no data to fill in");
                        return;
                    };
                    // The kinds of memory this buffer will accept, as a mask.
                    let size = allocating.lock().unwrap().size;
                    let allowed = (*data).type_;
                    if allowed & (1 << spa::sys::SPA_DATA_DmaBuf) != 0 {
                        match handing_out.lock().unwrap().pop() {
                            Some(target) => attach_dmabuf(data, target, &added),
                            None => tracing::warn!(
                                "screencast: pipewire asked for more than {BUFFERS} buffers"
                            ),
                        }
                    } else if allowed & (1 << spa::sys::SPA_DATA_MemFd) != 0 {
                        attach_shared(data, size, &added);
                    } else {
                        tracing::warn!("screencast: pipewire wants memory of kind {allowed}");
                    }
                }
            })
            .remove_buffer(move |_stream, (), raw| {
                // SAFETY: as above — and the buffer is still PipeWire's until
                // this returns, so the memory is dropped here and not later.
                unsafe {
                    let Some(data) = first_data(raw) else { return };
                    removed.lock().unwrap().remove(&(*data).fd);
                }
            })
            .register()
            .map_err(|e| anyhow::anyhow!("listening to a pipewire stream: {e}"))?;

        let described = offered_formats(*agreed.lock().unwrap())?;
        let mut params = described
            .iter()
            .map(|bytes| {
                spa::pod::Pod::from_bytes(bytes)
                    .ok_or_else(|| anyhow::anyhow!("the format description is not a valid pod"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        stream
            .connect(
                spa::utils::Direction::Output,
                None,
                // This end provides the memory, which is what ALLOC_BUFFERS
                // means. It has to: a buffer the GPU renders into is one only
                // the compositor can allocate, and PipeWire allocating for us
                // is how the shared-memory path worked and why it cost a
                // readback per frame.
                //
                // Nothing is mapped here — the shared-memory path maps its own
                // memory when it makes it, and a DMA-BUF is never touched by
                // the CPU at all.
                pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::ALLOC_BUFFERS,
                &mut params,
            )
            .map_err(|e| anyhow::anyhow!("connecting a pipewire stream: {e}"))?;

        // Ask the daemon to say when, and listen for it.
        //
        // The node id does not exist until the server has created it —
        // `node_id()` before that is 0xffffffff, which no client can connect
        // to — and the answer is dispatched on the thread loop's thread. This
        // used to be waited out on the compositor's own thread: first by
        // sleeping in five-millisecond slices, then by sleeping on a
        // condition variable woken by the answer itself. Either way the
        // desktop froze for the length of the wait, which is ordinarily a
        // millisecond and was allowed half a second — and the wait bought
        // nothing, because the only thing that needed the number, the portal
        // reply, can be sent from anywhere.
        //
        // Instead the round trip itself says when it is done. `sync` asks the
        // daemon for a `done` event once everything sent before it — the
        // stream's creation and connection included — has been processed, and
        // by then the node exists: proxy events are answered in order, so the
        // bound id is set before the done that follows it. The listener below
        // fills in the [`Arrival`] and keeps whatever promises were made on
        // it, and the portal reply leaves with them. The deadline that gives
        // up on a daemon which never answers lives with the rest of the
        // compositor's timers, in `state.rs`: refusing a share means taking
        // the stream back out, and that is the compositor's thread's job.
        //
        // The lock is held across the whole of this, which is what makes the
        // ordering safe: the answer is dispatched on the thread loop's
        // thread, which cannot touch anything until this returns.
        let seq = self
            .core
            .sync(0)
            .map_err(|e| anyhow::anyhow!("asking pipewire for a round trip: {e}"))?;
        let named = stream.clone();
        let arrival = Arrival::new();
        let announced_from = arrival.clone();
        let done_listener = self
            .core
            .add_listener_local()
            .done(move |id, done_seq| {
                if id != pw::core::PW_ID_CORE || done_seq.seq() != seq.seq() {
                    return;
                }
                announced_from.announced(named.node_id());
            })
            .register();

        Ok(Stream {
            node_id: INVALID_NODE,
            id: next_stream_id(),
            arrival,
            _done_listener: done_listener,
            stream,
            _listener: listener,
            size,
            streaming,
            last: None,
            agreed,
            pool,
            renegotiated: None,
            dmabuf,
            memory,
        })
    }
}

/// The first plane of a PipeWire buffer, if it has one.
///
/// One plane throughout: the buffer parameters ask for a single block, and a
/// format that needs more is one this compositor does not offer.
///
/// # Safety
/// `raw` must be a buffer PipeWire is handing over, valid for this call.
unsafe fn first_data(raw: *mut pw::sys::pw_buffer) -> Option<*mut spa::sys::spa_data> {
    if raw.is_null() {
        return None;
    }
    let buffer = (*raw).buffer;
    if buffer.is_null() || (*buffer).n_datas < 1 || (*buffer).datas.is_null() {
        return None;
    }
    Some((*buffer).datas)
}

/// Hand a buffer the GPU can draw into to PipeWire.
///
/// # Safety
/// `data` must be a plane PipeWire is asking this end to fill in.
unsafe fn attach_dmabuf(
    data: *mut spa::sys::spa_data,
    target: Dmabuf,
    held: &Arc<Mutex<HashMap<i64, Memory>>>,
) {
    let Some(layout) = Layout::of(&target) else {
        tracing::warn!("screencast: a target with no planes");
        return;
    };
    let Some(handle) = target.handles().next() else {
        tracing::warn!("screencast: a target with no descriptor");
        return;
    };
    let fd = handle.as_raw_fd() as i64;

    (*data).type_ = spa::sys::SPA_DATA_DmaBuf;
    (*data).flags = spa::sys::SPA_DATA_FLAG_READABLE;
    (*data).fd = fd;
    (*data).mapoffset = 0;
    (*data).maxsize = layout.size;
    // Never mapped: the consumer imports this into its own GPU, and nothing on
    // either end reads it with the CPU.
    (*data).data = std::ptr::null_mut();
    let chunk = (*data).chunk;
    if !chunk.is_null() {
        (*chunk).offset = layout.offset;
        (*chunk).stride = layout.stride;
        (*chunk).size = layout.size;
    }

    // Held until PipeWire gives the buffer back. The descriptor stays open for
    // as long as the target does, which is what the consumer is importing from.
    held.lock().unwrap().insert(fd, Memory::Dma(target));
}

/// Hand shared memory to PipeWire, for a consumer that cannot import a
/// DMA-BUF.
///
/// # Safety
/// `data` must be a plane PipeWire is asking this end to fill in.
unsafe fn attach_shared(
    data: *mut spa::sys::spa_data,
    size: Size<i32, Physical>,
    held: &Arc<Mutex<HashMap<i64, Memory>>>,
) {
    use smithay::reexports::rustix::{fs, mm};

    let stride = size.w.max(1) * 4;
    let len = (stride * size.h.max(1)) as usize;

    let fd = match fs::memfd_create("viewport-screencast", fs::MemfdFlags::CLOEXEC) {
        Ok(fd) => fd,
        Err(e) => {
            tracing::warn!("screencast: could not make memory for a buffer: {e}");
            return;
        }
    };
    if let Err(e) = fs::ftruncate(&fd, len as u64) {
        tracing::warn!("screencast: could not size a buffer: {e}");
        return;
    }
    // SAFETY: a fresh mapping of a descriptor made just above, unmapped once
    // when the buffer comes back.
    let ptr = match mm::mmap(
        std::ptr::null_mut(),
        len,
        mm::ProtFlags::READ | mm::ProtFlags::WRITE,
        mm::MapFlags::SHARED,
        &fd,
        0,
    ) {
        Ok(ptr) => ptr,
        Err(e) => {
            tracing::warn!("screencast: could not map a buffer: {e}");
            return;
        }
    };

    let raw = fd.as_raw_fd() as i64;
    (*data).type_ = spa::sys::SPA_DATA_MemFd;
    (*data).flags = 0;
    (*data).fd = raw;
    (*data).mapoffset = 0;
    (*data).maxsize = len as u32;
    // The compositor writes through this pointer, which is what makes the
    // frame appear at the other end of the descriptor.
    (*data).data = ptr;
    let chunk = (*data).chunk;
    if !chunk.is_null() {
        (*chunk).offset = 0;
        (*chunk).stride = stride;
        (*chunk).size = len as u32;
    }

    held.lock().unwrap().insert(
        raw,
        Memory::Shared {
            _fd: fd,
            ptr: ptr as usize,
            len,
        },
    );
}

/// Whether a negotiated format describes memory the GPU laid out.
///
/// The modifier is the tell: it says how the pixels are arranged in a buffer
/// the consumer will import, and a shared-memory format has no use for one.
fn carries_modifier(pod: &spa::pod::Pod) -> bool {
    use spa::pod::deserialize::PodDeserializer;

    match PodDeserializer::deserialize_any_from(pod.as_bytes()) {
        Ok((_, spa::pod::Value::Object(object))) => object
            .properties
            .iter()
            .any(|property| property.key == spa::sys::SPA_FORMAT_VIDEO_modifier),
        _ => false,
    }
}

/// Whether a source of this size needs the format agreed again.
///
/// Apart from the stream so it can be tested: getting it wrong in either
/// direction is invisible until a share has been running for a while — too
/// eager and every frame of a drag reallocates, too shy and the share freezes.
fn needs_renegotiation(
    agreed: Agreed,
    renegotiated: Option<std::time::Instant>,
    size: Size<i32, Physical>,
) -> bool {
    if size == agreed.size || size.w <= 0 || size.h <= 0 {
        return false;
    }
    renegotiated.is_none_or(|at| at.elapsed() >= Stream::SETTLE)
}

/// Both offers, in the order they are preferred.
///
/// A consumer takes the first it can use, and one that cannot import a DMA-BUF
/// — a remote desktop, a recorder without a GPU — still gets a picture rather
/// than a negotiation that fails.
fn offered_formats(agreed: Agreed) -> anyhow::Result<Vec<Vec<u8>>> {
    let mut described = Vec::new();
    if let Some(layout) = agreed.layout {
        described.push(video_format(agreed.size, Some(layout.modifier))?);
    }
    described.push(video_format(agreed.size, None)?);
    Ok(described)
}

/// The stream's format, as a SPA object.
///
/// Fixed rather than a range: the compositor knows exactly what it is going to
/// produce, and offering a choice it cannot satisfy only moves the failure to
/// the first frame.
fn video_format(size: Size<i32, Physical>, modifier: Option<u64>) -> anyhow::Result<Vec<u8>> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    use spa::pod::serialize::PodSerializer;
    use spa::pod::{object, property, Value};

    let mut object = object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx),
        property!(
            FormatProperties::VideoSize,
            Rectangle,
            spa::utils::Rectangle {
                width: size.w.max(1) as u32,
                height: size.h.max(1) as u32,
            }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Fraction,
            spa::utils::Fraction { num: 0, denom: 1 }
        ),
        // The rate the compositor will actually push at. A consumer sizes its
        // pipeline from this, and zero — "as it comes" — makes some of them
        // pick nothing at all.
        property!(
            FormatProperties::VideoMaxFramerate,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 }
        ),
    );

    // How the pixels are laid out in the buffer the consumer imports.
    //
    // Mandatory, as the protocol requires of a modifier: a consumer that
    // ignored it would import a buffer as though it were linear and show a
    // picture that is sheared, tiled, or noise. Exactly one value, because the
    // compositor has already allocated — this describes what exists rather
    // than asking what would be acceptable.
    if let Some(modifier) = modifier {
        object.properties.push(spa::pod::Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: spa::pod::PropertyFlags::MANDATORY,
            value: Value::Long(modifier as i64),
        });
    }

    let (cursor, _) = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map_err(|e| anyhow::anyhow!("describing the stream format: {e}"))?;
    Ok(cursor.into_inner())
}

/// What the consumer should expect of the buffers.
///
/// Published when the format is agreed. Without it a consumer has nothing to
/// allocate against and the stream never leaves Paused, which from the outside
/// is a share that hands back a node and then does nothing.
fn buffer_params(size: Size<i32, Physical>, dmabuf: Option<Layout>) -> anyhow::Result<Vec<u8>> {
    use spa::pod::serialize::PodSerializer;
    use spa::pod::Value;

    // The driver's own stride when the GPU allocated, and the packed width
    // otherwise. Telling a consumer the packed width for a buffer the driver
    // padded is a picture that shears further with every row.
    let (stride, total) = match dmabuf {
        Some(layout) => (layout.stride, layout.size as i32),
        None => {
            let stride = size.w.max(1) * 4;
            (stride, stride * size.h.max(1))
        }
    };
    // Built from the raw keys rather than a typed enum: the binding has names
    // for format properties and not for buffer ones, and the numbers are the
    // interface either way.
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            // Enough that the compositor can fill one while the consumer
            // reads another, few enough that a stalled consumer does not pin
            // much: each is a whole screen.
            spa::pod::Property::new(
                spa::sys::SPA_PARAM_BUFFERS_buffers,
                Value::Int(BUFFERS as i32),
            ),
            spa::pod::Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            spa::pod::Property::new(spa::sys::SPA_PARAM_BUFFERS_size, Value::Int(total)),
            spa::pod::Property::new(spa::sys::SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
            // What kind of memory the buffers are, as a choice of flags rather
            // than a bare number.
            //
            // A plain integer here is read as one value and not as the set of
            // kinds the stream will accept, and PipeWire answered by allocating
            // buffers it could put no memory in: every frame arrived with
            // maxsize zero, the compositor had nowhere to write, and a consumer
            // that connected and streamed saw an empty picture with nothing in
            // any log to say why.
            //
            // Whichever kind the format settled on, and only that one. Offering
            // both here and allocating one of them is the same mismatch again.
            spa::pod::Property::new(
                spa::sys::SPA_PARAM_BUFFERS_dataType,
                Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                    spa::utils::ChoiceFlags::empty(),
                    spa::utils::ChoiceEnum::Flags {
                        default: match dmabuf {
                            Some(_) => 1 << spa::sys::SPA_DATA_DmaBuf,
                            None => 1 << spa::sys::SPA_DATA_MemFd,
                        },
                        flags: vec![match dmabuf {
                            Some(_) => 1 << spa::sys::SPA_DATA_DmaBuf,
                            None => 1 << spa::sys::SPA_DATA_MemFd,
                        }],
                    },
                ))),
            ),
        ],
    };

    let (cursor, _) = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map_err(|e| anyhow::anyhow!("describing the stream buffers: {e}"))?;
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size() -> Size<i32, Physical> {
        (1920, 1080).into()
    }

    /// A promise to hear the answer, and where it landed.
    fn promised(arrival: &Arrival) -> Arc<std::sync::Mutex<Vec<u32>>> {
        let heard = Arc::new(std::sync::Mutex::new(Vec::new()));
        let taken = heard.clone();
        arrival.when_named(move |node| taken.lock().unwrap().push(node));
        heard
    }

    fn properties(bytes: &[u8]) -> Vec<spa::pod::Property> {
        let pod = spa::pod::Pod::from_bytes(bytes).expect("a pod");
        match spa::pod::deserialize::PodDeserializer::deserialize_any_from(pod.as_bytes()) {
            Ok((_, spa::pod::Value::Object(object))) => object.properties,
            other => panic!("not an object: {other:?}"),
        }
    }

    fn property(bytes: &[u8], key: u32) -> Option<spa::pod::Value> {
        properties(bytes)
            .into_iter()
            .find(|property| property.key == key)
            .map(|property| property.value)
    }

    /// The modifier says how the pixels are laid out in memory the consumer
    /// imports. A DMA-BUF offer without one is a buffer the consumer reads as
    /// though it were linear, which is a picture of noise.
    #[test]
    fn a_dmabuf_format_carries_its_modifier() {
        let described = video_format(size(), Some(0x0100_0000_0000_0001)).expect("a format");
        let value = property(&described, spa::sys::SPA_FORMAT_VIDEO_modifier);
        assert_eq!(value, Some(spa::pod::Value::Long(0x0100_0000_0000_0001)));

        let pod = spa::pod::Pod::from_bytes(&described).expect("a pod");
        assert!(carries_modifier(pod));
    }

    /// And the shared-memory offer must not: it is what a consumer that cannot
    /// import a DMA-BUF falls back to, and the modifier is how the two are
    /// told apart once one has been chosen.
    #[test]
    fn a_shared_memory_format_carries_none() {
        let described = video_format(size(), None).expect("a format");
        assert_eq!(
            property(&described, spa::sys::SPA_FORMAT_VIDEO_modifier),
            None
        );

        let pod = spa::pod::Pod::from_bytes(&described).expect("a pod");
        assert!(!carries_modifier(pod));
    }

    /// One kind of memory, matching the format that was agreed.
    ///
    /// Offering both and allocating one is what left every buffer with a
    /// maxsize of zero: the stream negotiated, played, and delivered frames
    /// with nowhere to write them.
    #[test]
    fn the_buffers_are_of_the_kind_that_was_negotiated() {
        let kinds = |params: &[u8]| match property(params, spa::sys::SPA_PARAM_BUFFERS_dataType) {
            Some(spa::pod::Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                _,
                spa::utils::ChoiceEnum::Flags { flags, .. },
            )))) => flags,
            other => panic!("not a choice of flags: {other:?}"),
        };

        let layout = Layout {
            modifier: 0,
            stride: 7680,
            offset: 0,
            size: 7680 * 1080,
        };
        let drawn = buffer_params(size(), Some(layout)).expect("buffer parameters");
        assert_eq!(kinds(&drawn), vec![1 << spa::sys::SPA_DATA_DmaBuf]);

        let copied = buffer_params(size(), None).expect("buffer parameters");
        assert_eq!(kinds(&copied), vec![1 << spa::sys::SPA_DATA_MemFd]);
    }

    /// A source that has changed size needs the format agreed again, and one
    /// that has not must not be renegotiated at all: doing it per frame would
    /// allocate three screens' worth of buffers sixty times a second.
    #[test]
    fn only_a_resize_asks_for_a_new_format() {
        let agreed = Agreed {
            size: (1920, 1080).into(),
            layout: None,
        };
        assert!(!needs_renegotiation(agreed, None, (1920, 1080).into()));
        assert!(needs_renegotiation(agreed, None, (1280, 720).into()));

        // Nothing has a size of zero except a window on its way out, and
        // renegotiating to it would agree a format no frame can satisfy.
        assert!(!needs_renegotiation(agreed, None, (0, 0).into()));
        assert!(!needs_renegotiation(agreed, None, (1280, 0).into()));
    }

    /// And a drag is a new size every frame. Each one costs three buffers and
    /// a round trip with the consumer, so the source has to hold still first.
    #[test]
    fn a_dragged_edge_settles_before_the_format_moves() {
        let agreed = Agreed {
            size: (1920, 1080).into(),
            layout: None,
        };
        let just_now = std::time::Instant::now();
        assert!(!needs_renegotiation(
            agreed,
            Some(just_now),
            (1280, 720).into()
        ));

        let a_while_ago = just_now - Stream::SETTLE - std::time::Duration::from_millis(1);
        assert!(needs_renegotiation(
            agreed,
            Some(a_while_ago),
            (1280, 720).into()
        ));
    }

    /// The driver's stride, not the packed width.
    ///
    /// A consumer told the packed width for a buffer the driver padded reads
    /// each row a little further into the next one, which shears the picture
    /// progressively down the screen.
    #[test]
    fn a_drawn_buffer_is_described_with_the_stride_it_has() {
        let layout = Layout {
            modifier: 0,
            stride: 8192,
            offset: 0,
            size: 8192 * 1080,
        };
        let drawn = buffer_params(size(), Some(layout)).expect("buffer parameters");
        assert_eq!(
            property(&drawn, spa::sys::SPA_PARAM_BUFFERS_stride),
            Some(spa::pod::Value::Int(8192))
        );
        assert_eq!(
            property(&drawn, spa::sys::SPA_PARAM_BUFFERS_size),
            Some(spa::pod::Value::Int(8192 * 1080))
        );

        // And the packed width when the compositor is copying instead.
        let copied = buffer_params(size(), None).expect("buffer parameters");
        assert_eq!(
            property(&copied, spa::sys::SPA_PARAM_BUFFERS_stride),
            Some(spa::pod::Value::Int(1920 * 4))
        );
    }

    /// The promise is kept once the node arrives, and only ever once: a
    /// second `done` for the same round trip cannot happen, but the code
    /// that would ignore one is what keeps a duplicate answer from reaching
    /// an application that has already been told.
    #[test]
    fn the_node_is_announced_once_to_whoever_asked() {
        let arrival = Arrival::new();
        let heard = promised(&arrival);
        assert_eq!(arrival.status(), Arrived::Yet);

        arrival.announced(41);
        arrival.announced(42);

        assert_eq!(*heard.lock().unwrap(), vec![41]);
        assert_eq!(arrival.status(), Arrived::Now(41));
    }

    /// A promise made of an answer already in hand is kept on the spot,
    /// which is what lets the caller arm its reply without caring whether
    /// the daemon got there first.
    #[test]
    fn a_promise_made_after_the_answer_is_kept_at_once() {
        let arrival = Arrival::new();
        arrival.announced(7);

        let heard = promised(&arrival);

        assert_eq!(*heard.lock().unwrap(), vec![7]);
    }

    /// The deadline claiming the refusal silences everything else: a `done`
    /// event landing after it finds the cell filled with failure and says
    /// nothing, and the application gets one answer — the refusal — rather
    /// than a refusal followed by a node nobody can use.
    #[test]
    fn a_claimed_deadline_leaves_nothing_for_the_daemon_to_say() {
        let arrival = Arrival::new();
        assert!(arrival.fail());

        let heard = promised(&arrival);
        arrival.announced(9);

        assert!(heard.lock().unwrap().is_empty());
        assert_eq!(arrival.status(), Arrived::Never);
        // And the refusal is still owed: the success never left, so the
        // deadline that claimed it is the one that has to send it.
        assert!(arrival.fail());
    }

    /// A daemon that answers without a node has failed the share in its own
    /// way. Nobody is told from here — the promises are success-only — and
    /// the refusal stays the deadline's to deliver.
    #[test]
    fn a_daemon_that_answers_without_a_node_says_nothing_here() {
        let arrival = Arrival::new();
        let heard = promised(&arrival);

        arrival.announced(INVALID_NODE);

        assert!(heard.lock().unwrap().is_empty());
        assert_eq!(arrival.status(), Arrived::Never);
        assert!(arrival.fail());
    }

    /// But a stream that succeeded first owes nothing to the deadline: its
    /// claim comes back empty, and the teardown it would have done never
    /// happens to a stream somebody is about to watch.
    #[test]
    fn a_node_that_arrived_first_cannot_be_claimed() {
        let arrival = Arrival::new();
        arrival.announced(5);

        assert!(!arrival.fail());
        assert_eq!(arrival.status(), Arrived::Now(5));
    }
}
