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

use std::io::Cursor;

use pipewire as pw;
use pw::spa;
use smithay::utils::{Physical, Size};

/// PipeWire's own file descriptor, so calloop can wait on it.
///
/// Wrapped because calloop wants something that owns its descriptor and
/// PipeWire's belongs to the loop; this borrows it for the lifetime of the
/// source, which is the lifetime of the connection.
pub struct LoopFd(pub std::os::fd::RawFd);

impl std::os::fd::AsFd for LoopFd {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        // SAFETY: the descriptor belongs to the PipeWire loop, which outlives
        // the event source: dropping the connection removes the source first.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(self.0) }
    }
}

/// A stream a client is watching.
pub struct Stream {
    pub stream: pw::stream::Stream,
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
    /// The node a client connects to, which is what the portal hands back over
    /// D-Bus.
    pub node_id: u32,
}

impl Stream {
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

    pub fn push(&mut self, pixels: &[u8], size: Size<i32, Physical>, loop_: &pw::thread_loop::ThreadLoop) {
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
                return;
            };
            let Some(destination) = data.data() else {
                return;
            };
            if destination.len() < wanted || pixels.len() < wanted {
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

/// The compositor's connection to PipeWire.
///
/// On a thread of its own rather than inside the compositor's event loop.
/// Attaching PipeWire's loop to calloop stopped the compositor dispatching
/// anything at all — no redraws, no input, no clients — from the moment a
/// stream was created, and a screen share is not worth a frozen desktop. The
/// thread loop is what xdg-desktop-portal-wlr uses for the same reason: every
/// call into it takes the loop's own lock, so frames can be handed over from
/// wherever the renderer happens to be.
pub struct Pipewire {
    pub thread_loop: pw::thread_loop::ThreadLoop,
    pub core: pw::core::Core,
    /// Held because dropping it tears the connection down.
    _context: pw::context::Context,
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
        let thread_loop = unsafe {
            pw::thread_loop::ThreadLoop::new(Some("viewport-screencast"), None)
        }
        .map_err(|e| anyhow::anyhow!("creating a pipewire loop: {e}"))?;

        // Everything below runs with the loop held, which is the contract:
        // the thread is already dispatching, and touching its objects without
        // the lock is a race with it.
        let (context, core) = {
            let _guard = thread_loop.lock();
            let context = pw::context::Context::new(&thread_loop)
                .map_err(|e| anyhow::anyhow!("creating a pipewire context: {e}"))?;
            let core = context
                .connect(None)
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
    pub fn create_stream(&self, name: &str, size: Size<i32, Physical>) -> anyhow::Result<Stream> {
        let mut guard = self.thread_loop.lock();
        // The node id is what the portal hands back, and it does not exist
        // until the server has answered — `node_id()` before that is
        // 0xffffffff, which a client cannot connect to.
        const INVALID_NODE: u32 = u32::MAX;
        let stream = pw::stream::Stream::new(
            &self.core,
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

        let streaming = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = streaming.clone();
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

                match buffer_params(size) {
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
            .register()
            .map_err(|e| anyhow::anyhow!("listening to a pipewire stream: {e}"))?;

        let format = video_format(size)?;
        let mut params = [spa::pod::Pod::from_bytes(&format)
            .ok_or_else(|| anyhow::anyhow!("the format description is not a valid pod"))?];

        stream
            .connect(
                spa::utils::Direction::Output,
                None,
                // Mapped, because the frames are written by the CPU after a
                // readback. A DMA-BUF stream avoids the copy and is what this
                // should become once the format negotiation carries
                // modifiers.
                pw::stream::StreamFlags::DRIVER
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::ALLOC_BUFFERS,
                &mut params,
            )
            .map_err(|e| anyhow::anyhow!("connecting a pipewire stream: {e}"))?;

        // Wait for it, briefly. The loop's own thread is what answers, so
        // this releases the lock between looks rather than iterating.
        let mut node_id = stream.node_id();
        for _ in 0..100 {
            if node_id != INVALID_NODE {
                break;
            }
            drop(guard);
            std::thread::sleep(std::time::Duration::from_millis(5));
            guard = self.thread_loop.lock();
            node_id = stream.node_id();
        }
        drop(guard);
        if node_id == INVALID_NODE {
            anyhow::bail!("pipewire did not give the stream a node");
        }

        Ok(Stream {
            node_id,
            stream,
            _listener: listener,
            size,
            streaming,
            last: None,
        })
    }
}

/// The stream's format, as a SPA object.
///
/// Fixed rather than a range: the compositor knows exactly what it is going to
/// produce, and offering a choice it cannot satisfy only moves the failure to
/// the first frame.
fn video_format(size: Size<i32, Physical>) -> anyhow::Result<Vec<u8>> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    use spa::pod::serialize::PodSerializer;
    use spa::pod::{object, property, Value};

    let object = object!(
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

    let (cursor, _) = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map_err(|e| anyhow::anyhow!("describing the stream format: {e}"))?;
    Ok(cursor.into_inner())
}

/// What the consumer should expect of the buffers.
///
/// Published when the format is agreed. Without it a consumer has nothing to
/// allocate against and the stream never leaves Paused, which from the outside
/// is a share that hands back a node and then does nothing.
fn buffer_params(size: Size<i32, Physical>) -> anyhow::Result<Vec<u8>> {
    use spa::pod::serialize::PodSerializer;
    use spa::pod::Value;

    let stride = size.w.max(1) * 4;
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
            spa::pod::Property::new(spa::sys::SPA_PARAM_BUFFERS_buffers, Value::Int(3)),
            spa::pod::Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            spa::pod::Property::new(
                spa::sys::SPA_PARAM_BUFFERS_size,
                Value::Int(stride * size.h.max(1)),
            ),
            spa::pod::Property::new(spa::sys::SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
            // Shared memory, which is what the frames are copied into today.
            // Rendering into a DMA-BUF the consumer imports is what this
            // should become, and this is the line that will say so.
            spa::pod::Property::new(
                spa::sys::SPA_PARAM_BUFFERS_dataType,
                Value::Int((1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr)),
            ),
        ],
    };

    let (cursor, _) = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map_err(|e| anyhow::anyhow!("describing the stream buffers: {e}"))?;
    Ok(cursor.into_inner())
}
