// SPDX-License-Identifier: GPL-3.0-or-later
//
// The real backend: DRM/KMS out, libinput in, udev to find the devices,
// libseat so none of it needs to be root. Ports the DRM half of src/output.c
// and src/session.c.
//
// This is where the Vulkan renderer actually gets used. Under winit the output
// buffer belongs to winit's swapchain and there is no dmabuf to hand it; here
// the compositor allocates its own scanout buffers through GBM and binds them
// as dmabufs, which is the path `Bind<Dmabuf>` was written for.
//
// What is deliberately not here yet: multi-GPU, and hotplug of whole devices.
// Connector hotplug within the primary device is handled, because plugging a
// monitor in is ordinary and unplugging a GPU is not.

use std::collections::HashMap;

use anyhow::{anyhow, Context as _, Result};
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::compositor::FrameFlags;
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::output::{DrmOutput, DrmOutputManager};
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType};
use smithay::backend::input::InputEvent;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::drm::control::{connector, crtc, Device as _, ModeTypeFlags};
use smithay::reexports::rustix::fs::OFlags;
use smithay::utils::DeviceFd;

use viewport_vulkan::{Device as VulkanDevice, VulkanRenderer};

use crate::state::ViewportState;

/// What `VIEWPORT_GPU` asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Nothing was asked for; rank the candidates as usual.
    Ranked,
    /// This one, by index into the candidates.
    Named(usize),
    /// Something was asked for and no GPU matches it.
    NotFound,
}

/// Which candidate a `VIEWPORT_GPU` value names.
///
/// Matched as a substring of the device path, so `card1`, `renderD129` and a
/// whole `/dev/dri/by-path/...` all work — nobody remembers which form a
/// particular machine calls its GPUs, and the useful ones are not
/// interchangeable between reboots.
///
/// `NotFound` rather than silently ranking, because a value that matches
/// nothing is a mistake worth reporting: the alternative is a hybrid laptop
/// running on the wrong GPU while the variable that was supposed to fix it
/// sits in the environment doing nothing.
/// The node clients should allocate on for this card.
///
/// A card usually has a render node beside its primary one — `card1` and
/// `renderD128` — and that is the node the renderer draws with and the one
/// advertised over `linux-dmabuf` as the device to allocate on.
///
/// Some have none. A virtual DRM device has none, and neither does a
/// display controller on a machine where scanout and rendering are separate
/// chips. The primary node is not a substitute: the compositor can use it
/// because it holds DRM master, and a client cannot open it at all. Handing
/// its `dev_id` to `DmabufFeedbackBuilder` therefore tells every client to
/// allocate on a device it is not allowed to touch, and what comes back is
///
///     MESA-EGL: warning: failed to get driver name for fd -1
///
/// from the client, naming nothing about this compositor. Falling back
/// silently is what made that the visible symptom, so it is said out loud —
/// the fallback is kept, because the compositor's own use of the node does
/// work and refusing to start would be worse.
pub fn client_render_node(card: &DrmNode) -> DrmNode {
    match card
        .node_with_type(NodeType::Render)
        .and_then(|node| node.ok())
    {
        Some(render) => render,
        None => {
            tracing::warn!(
                "{card:?} has no render node, so clients are being told to \
                 allocate on its primary node — which they cannot open. \
                 Expect every GL and Vulkan client to fail at startup with a \
                 driver error of its own. A card that only scans out needs the \
                 rendering GPU's render node advertised instead, which this \
                 does not yet do."
            );
            *card
        }
    }
}

/// The items, in the order they first appear, with later repeats dropped.
///
/// Order is the point: the seat's primary is put in front of the full GPU list
/// so the ranking sees it first, and sorting or hashing to deduplicate would
/// throw that away. The lists here are two or three long, so the quadratic
/// comparison is cheaper than the set that would avoid it.
fn first_occurrences<T: PartialEq>(items: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut kept: Vec<T> = Vec::new();
    for item in items {
        if !kept.contains(&item) {
            kept.push(item);
        }
    }
    kept
}

pub fn gpu_named(paths: &[Option<String>], wanted: Option<&str>) -> Choice {
    let Some(wanted) = wanted.map(str::trim).filter(|w| !w.is_empty()) else {
        return Choice::Ranked;
    };
    match paths
        .iter()
        .position(|path| path.as_deref().is_some_and(|p| p.contains(wanted)))
    {
        Some(at) => Choice::Named(at),
        None => Choice::NotFound,
    }
}

/// The formats a scanout buffer may use, in preference order.
///
/// Ten-bit first because it is strictly better where the display takes it, and
/// because HDR needs more than eight bits per channel. Eight-bit is the
/// fallback every display supports.
const SCANOUT_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Argb8888,
    Fourcc::Xrgb8888,
];

/// The same list with the ten-bit half removed.
const SCANOUT_FORMATS_8: &[Fourcc] = &[Fourcc::Argb8888, Fourcc::Xrgb8888];

/// And with the eight-bit fallback removed, so a display that cannot do ten
/// bits fails to come up rather than quietly giving eight.
const SCANOUT_FORMATS_10: &[Fourcc] = &[Fourcc::Abgr2101010, Fourcc::Argb2101010];

/// Which of those lists this session asked for.
///
/// `$VIEWPORT_PIXEL_FORMAT`, which `--pixel-format` and the config file's
/// `pixel_format` both write before a device is opened — the list is needed
/// while the DRM device is being built, before there is any state to read a
/// setting out of, and one variable is what `--renderer` already does.
///
/// An unparseable value is a warning and the default rather than a failure to
/// start: a compositor that refuses to bring the screens up over a typo in an
/// environment variable leaves a TTY as the only way to fix it.
fn scanout_formats() -> &'static [Fourcc] {
    let asked = std::env::var("VIEWPORT_PIXEL_FORMAT").unwrap_or_default();
    match crate::config::parse_pixel_format(&asked) {
        Ok(crate::config::PixelFormat::Auto) => SCANOUT_FORMATS,
        Ok(crate::config::PixelFormat::Eight) => {
            tracing::info!("VIEWPORT_PIXEL_FORMAT={asked}: eight bits per channel");
            SCANOUT_FORMATS_8
        }
        Ok(crate::config::PixelFormat::Ten) => {
            tracing::info!(
                "VIEWPORT_PIXEL_FORMAT={asked}: ten bits per channel, with no \
                 eight-bit fallback — an output whose plane will not take it \
                 stays dark"
            );
            SCANOUT_FORMATS_10
        }
        Err(e) => {
            tracing::warn!("{e}; using the default order");
            SCANOUT_FORMATS
        }
    }
}

// The element list is `crate::render::OutputElement`, generic over the
// renderer so the DRM path can use either.

type Exporter = GbmFramebufferExporter<DrmDeviceFd>;

/// What rides along with a queued frame: the clients waiting to be told when
/// it reached the screen.
type Feedback = Option<smithay::desktop::utils::OutputPresentationFeedback>;

type Manager = DrmOutputManager<GbmAllocator<DrmDeviceFd>, Exporter, Feedback, DrmDeviceFd>;

/// One CRTC being driven.
pub struct Surface {
    pub output: Output,
    /// The connector this CRTC is driving, so a second pass can tell an
    /// output that is already up from one that still needs a CRTC.
    pub connector: connector::Handle,
    pub drm_output: DrmOutput<GbmAllocator<DrmDeviceFd>, Exporter, Feedback, DrmDeviceFd>,
    /// The global handed to clients, dropped when the connector goes.
    _global: smithay::reexports::wayland_server::backend::GlobalId,
    /// Whether anything has been put on this output yet. Logged once, because
    /// "did it draw at all" is the first question of any bring-up and the
    /// answer was not in the log the first time.
    drawn: bool,
    /// Whether a composite of this output has been written already.
    dumped: bool,
    /// Whether this output has been switched into HDR.
    pub hdr: bool,
    /// Whether this output's backlight is on. Off means DPMS off — the panel
    /// sleeps — while the output keeps its place in the layout.
    pub powered: bool,
    /// Whether this output's frames are currently allowed to tear, so the
    /// compositor is only told when it changes rather than once a frame.
    pub tearing: bool,
    /// How many tearing flips this display has refused. A frame or two around
    /// the change itself is ordinary; a display that keeps refusing cannot.
    pub tearing_failures: u32,
    /// Set when a tearing flip was refused by the display. The capability says
    /// the hardware can flip asynchronously, not that it can in whatever state
    /// this output is in — adaptive sync being the combination that goes wrong
    /// — and asking again every frame would be a screen that stops once per
    /// frame for the rest of the session.
    pub refuses_tearing: bool,
    /// Whether this output is switched on.
    ///
    /// A client can turn a monitor off through wlr-output-management — kanshi
    /// does it to the laptop panel when a dock appears. A disabled output keeps
    /// its CRTC and its surface, so turning it back on is a commit rather than
    /// a re-scan of the device, but nothing is drawn on it and its planes are
    /// cleared so the panel actually sleeps.
    pub enabled: bool,
    /// What a client should allocate against for a surface on this output,
    /// including which formats could go straight to a display plane.
    ///
    /// Per output rather than per GPU, because the scanout half is per CRTC:
    /// the formats come from this output's primary plane, and a neighbouring
    /// monitor on the same card can have a different set.
    pub feedback: Option<smithay::wayland::dmabuf::DmabufFeedback>,
    /// A frame is queued and has not been scanned out yet.
    ///
    /// One frame in flight per output, which is what anvil arranges by
    /// scheduling the next repaint from the vblank rather than rendering
    /// straight away (`anvil/src/udev.rs:1328`). Rendering again before the
    /// flip draws into the next swapchain buffer with a damage age that no
    /// longer describes it, so the buffers end up holding different pictures —
    /// visible as flicker whenever a client repaints quickly, like a terminal
    /// being typed into.
    pub pending: bool,
    /// When that frame was queued.
    ///
    /// `pending` alone says a flip is out; this says how long it has been out,
    /// which is the difference between a screen that is about to update and one
    /// that has stopped. A GPU reset eats the queued flip and the vblank that
    /// would clear `pending` never arrives, so without a clock on it the output
    /// is frozen for the rest of the session with nothing in the log. See
    /// [`crate::recovery`].
    pub queued_at: Option<std::time::Instant>,
}

/// Everything the DRM backend holds.
/// The renderer the DRM path draws with.
///
/// Vulkan wherever it can: it is the one with colour management and explicit
/// sync. GLES exists for the machines Vulkan cannot serve — a virtual machine,
/// where software Vulkan lacks `VK_EXT_image_drm_format_modifier` and Venus
/// aborts inside its own driver, while virgl gives OpenGL ES on the GBM
/// platform and works. Without this a guest shows nothing at all.
// 352 bytes against a boxed 8 is the remaining spread, and clippy still asks.
// Boxing Vulkan too would put an indirection on every renderer access in the
// path that runs every frame, to save a memcpy that is already down from 6432
// bytes to 352.
#[allow(clippy::large_enum_variant)]
pub enum Gpu {
    Vulkan(VulkanRenderer),
    /// Boxed because `GlesRenderer` is 6432 bytes against Vulkan's 352, and an
    /// enum is as large as its largest variant. Every render pass moves this
    /// out and back — see `Placeholder` — so the difference is a memcpy per
    /// frame per output, not a one-off.
    Gles(Box<smithay::backend::renderer::gles::GlesRenderer>),
    /// Only ever here while the real renderer is on the stack: a call that
    /// borrows the rest of `Udev` cannot borrow the renderer out of it at the
    /// same time, so it is moved out and put back.
    Placeholder,
}

/// What a renderer composites a capture into.
///
/// Vulkan can allocate a DMA-BUF and hand it to a client; GLES draws into a
/// renderbuffer and the pixels are read back, which is what the nested backend
/// has always done. The difference is one associated type rather than two
/// copies of the capture paths.
pub trait Captures: smithay::backend::renderer::RendererSuper {
    type Buffer: Send + 'static;
}

impl Captures for VulkanRenderer {
    type Buffer = smithay::backend::allocator::dmabuf::Dmabuf;
}

impl Captures for smithay::backend::renderer::gles::GlesRenderer {
    type Buffer = smithay::backend::renderer::gles::GlesRenderbuffer;
}

/// Do the same thing with whichever renderer this is.
///
/// The body is written once and compiled twice, which is what keeps the two
/// paths honest: anything that works for one has to type-check for the other.
#[macro_export]
macro_rules! with_gpu {
    ($gpu:expr, |$renderer:ident| $body:expr) => {
        match $gpu {
            $crate::udev::Gpu::Vulkan($renderer) => $body,
            // Reborrowed through the `Box` so the body sees `&mut GlesRenderer`
            // and not `&mut Box<GlesRenderer>` — `Box` forwards inherent
            // methods but not trait impls, so without this the two arms would
            // stop type-checking against the same body.
            $crate::udev::Gpu::Gles(boxed) => {
                let $renderer = &mut **boxed;
                $body
            }
            $crate::udev::Gpu::Placeholder => {
                panic!("the renderer was used while it was moved out")
            }
        }
    };
}

impl Gpu {
    /// Throw away every cached import, whichever renderer this is.
    ///
    /// A renderer caches the images it made from client buffers, because
    /// re-importing one every frame is the cost of the import path doubled.
    /// The entries are keyed by the client's buffer and dropped when it goes,
    /// which is right until the images themselves stop describing anything:
    /// after a GPU reset the queue state the import baked in — the barrier
    /// that took the image from the foreign queue, the layout it was left in —
    /// is gone, while every client buffer is still alive and still keyed to
    /// the stale image. Waiting for the buffers to die would never clear them.
    ///
    /// Not a lost surface: the next commit imports again. It costs one import
    /// per mapped surface, once, on a path that has just finished resetting a
    /// graphics card.
    ///
    /// Reported rather than returned. Every caller is a recovery step, and a
    /// step that cannot flush a cache still wants to run the rest of itself.
    pub fn invalidate_caches(&mut self) {
        use smithay::backend::renderer::Renderer as _;
        // Stringified inside the macro because the two arms have different
        // error types, and the body is compiled once for each.
        let result = with_gpu!(self, |renderer| renderer
            .invalidate_caches()
            .map_err(|e| e.to_string()));
        if let Err(e) = result {
            tracing::warn!("could not invalidate the renderer's caches: {e}");
        }
    }

    /// The formats a client may hand over, whichever renderer this is.
    pub fn dmabuf_formats(&self) -> smithay::backend::allocator::format::FormatSet {
        use smithay::backend::renderer::ImportDma as _;
        match self {
            Gpu::Vulkan(renderer) => renderer.dmabuf_formats(),
            Gpu::Gles(renderer) => renderer.dmabuf_formats(),
            Gpu::Placeholder => Default::default(),
        }
    }
}

/// Which output, across every GPU.
///
/// A `crtc::Handle` is only unique within the device that issued it, so two
/// GPUs routinely hand out the same value. Keyed on the handle alone — which
/// is what this was before there was more than one device — a vblank from the
/// second GPU redraws an output on the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId {
    /// Index into `Udev::devices`.
    ///
    /// Stable for the life of the session. A GPU that goes away keeps its slot
    /// and comes back into it — see [`crate::recovery`] — because every vblank
    /// closure has captured the index it was inserted with, so the list may
    /// grow but must never shift.
    pub device: usize,
    pub crtc: crtc::Handle,
}

/// One GPU, and everything hanging off it.
///
/// Every GPU gets its own renderer rather than one renderer drawing for all of
/// them. A buffer is only cheap on the device that allocated it, so the
/// alternative is rendering on the primary and copying each frame across PCIe
/// to whatever is scanning it out.
pub struct Device {
    pub node: DrmNode,
    /// The render node, for the Vulkan device and for dmabuf feedback.
    pub render_node: DrmNode,
    pub renderer: Gpu,
    /// The GBM device this GPU's renderer was built on, kept so the renderer
    /// can be built again. A Vulkan device that turns out not to be able to
    /// drive this display is only discovered once an output is attempted, and
    /// by then the device that made it is inside the manager.
    pub gbm: GbmDevice<DrmDeviceFd>,
    pub manager: Manager,
    pub surfaces: HashMap<crtc::Handle, Surface>,
    /// What a client should allocate against for a surface being shown on this
    /// GPU.
    ///
    /// The global carries one default feedback naming the primary, which is
    /// the right answer while there is one GPU and the wrong one the moment
    /// there are two: a window on the second card would be told to allocate
    /// for the first, and the renderer that has to import it is the second's.
    /// Per surface, a client is told the device actually displaying it.
    ///
    /// `None` if the feedback could not be built, which costs the per-surface
    /// hint and nothing else — the default global still applies.
    pub feedback: Option<smithay::wayland::dmabuf::DmabufFeedback>,
    /// Whether this slot currently holds a card that answers.
    ///
    /// False between a GPU being unregistered and the same GPU coming back.
    /// The slot stays — `OutputId::device` is an index into this list — and the
    /// dead device stays in it until a working one replaces it, because there
    /// is no such thing as an empty `DrmOutputManager`. Nothing touches it
    /// meanwhile: the scan skips it, the session handler skips it, and it has
    /// no surfaces because they were dropped when the card went.
    pub online: bool,
    /// The bus address this card sits at, from sysfs.
    ///
    /// The one name for a GPU that survives it being unregistered and
    /// registered again. See [`crate::recovery::bus_id_of`].
    pub bus: Option<String>,
    /// The loop registration watching this device's vblanks.
    ///
    /// Kept so it can be taken out again. A `Generic` source on an fd the
    /// kernel has revoked polls ready for ever, which is a compositor burning a
    /// core over a GPU that is not there — so a card that goes stops being
    /// watched the moment we hear about it.
    pub vblank: Option<smithay::reexports::calloop::RegistrationToken>,
    /// Where this device is in the recovery ladder. See [`crate::recovery`].
    pub recovery: crate::recovery::Step,
    /// Commits or renders that have failed since the last vblank.
    pub errors: u32,
    /// When the current recovery step was taken, so the next one is not tried
    /// before the hardware has had a chance. See [`crate::recovery::DWELL`].
    pub stepped_at: Option<std::time::Instant>,
    /// Watchdog ticks left in which to re-scan a card that has just come back.
    ///
    /// A DRM device is announced before its connectors are necessarily
    /// readable, and a scan that came up empty is repeated by nothing — no
    /// surface means no flip, no flip means no vblank, and the frame clock does
    /// not scan. Counted down rather than waited on once, because how long the
    /// driver takes is not something to guess at.
    pub settle: u32,
    /// How many times this card has been reopened without coming back, which is
    /// what the retry backoff is measured in.
    pub attempts: u32,
    /// When it went, so the backoff has something to count from.
    pub offline_since: Option<std::time::Instant>,
}

pub struct Udev {
    /// `wp-drm-lease-v1`: handing a whole connector to a client.
    ///
    /// A headset is not a monitor — the compositor cannot composite for it,
    /// because the client is the only thing that knows how to warp for the
    /// lenses and when to submit for the display's own timing. So the
    /// connector is leased out whole and the compositor stops touching it.
    ///
    /// `None` if the global could not be created, which is not fatal: it
    /// leaves a session where nothing can lease, and everything else works.
    pub lease_state: Option<smithay::wayland::drm_lease::DrmLeaseState>,
    /// Leases handed out. Dropping one revokes it, so they are kept here for
    /// as long as the client holds them.
    pub leases: Vec<smithay::wayland::drm_lease::DrmLease>,
    pub session: LibSeatSession,
    /// Every GPU being driven. `devices[0]` is the one the shell and the
    /// clients are told about — where buffers are allocated by default — and
    /// the rest drive their own outputs with their own renderers.
    pub devices: Vec<Device>,
    /// True while the outputs are off because the session went idle.
    pub blanked: bool,
    /// False between a VT switch away and the switch back. Every device fd is
    /// revoked in that window, so committing a frame would fail.
    pub active: bool,
    /// Frame pacing counters. `None` unless `VIEWPORT_FRAME_LOG` is set.
    pub frame_log: Option<FrameLog>,
    /// When the last vblank arrived, on any output.
    ///
    /// Read by the frame clock to tell whether it is needed. A vblank chain
    /// that is running invites clients at exactly the refresh rate, and the
    /// clock inviting them again on top of that is not a safety net, it is a
    /// second invitation: the client paints twice per frame and the
    /// compositor shows one of them. Measured on a 60Hz screen, a client
    /// drawing 120 frames a second and half of them thrown away.
    pub last_vblank: Option<std::time::Instant>,
    /// The last vblank on each output, by name, rather than the newest on any
    /// of them.
    ///
    /// `last_vblank` above is one timestamp for the whole device, and the
    /// barrier tick used to defer to it — but the release it defers to is
    /// per-output. One screen flipping kept the global stamp fresh and so
    /// silenced the tick for every other screen, whose windows that flip never
    /// walks. See `release_barriers`.
    pub last_vblank_by_output: HashMap<String, std::time::Instant>,
    /// Whether a client commit has been applied since the last flip reached
    /// the screen. Measurement only, for `empty_after_commit`.
    pub committed_since_flip: bool,
    /// When the first client commit since the last flip arrived.
    ///
    /// Measurement only, for `commit_to_flip`. The first rather than the
    /// newest: a client that commits twice before either is drawn has waited
    /// since the first.
    pub first_commit_at: Option<std::time::Instant>,
    /// Cards that have never opened, waiting to be tried again.
    ///
    /// A GPU this session has not seen has no slot in `devices` to hang the
    /// retry state on, so a first open that fails would otherwise leave nothing
    /// behind at all — and `UdevEvent::Added` fires once per devnum, so nothing
    /// asks again. An eGPU, or a card whose driver was still binding when udev
    /// announced it, was dropped for the rest of the session. The watchdog
    /// drains this on the same backoff it retries an offline card with.
    pub pending_adds: Vec<PendingAdd>,
}

/// A card that announced itself and would not open.
#[derive(Debug, Clone, Copy)]
pub struct PendingAdd {
    /// The primary node to open, as udev named it.
    pub card: DrmNode,
    /// How many times it has been tried, which is what the backoff counts in.
    pub attempts: u32,
    /// When it was last tried, for the backoff to measure from.
    pub tried_at: std::time::Instant,
}

/// Why a client on a 240Hz screen gets 204 frames a second.
///
/// Counting rather than reasoning, because reasoning has already been wrong
/// once here: moving frame callbacks onto the vblank was supposed to fix the
/// shortfall and changed it by 0.3%.
///
/// The question these answer is which of two stories is true. Either the
/// vblanks arrive at the refresh rate and something drops frames between
/// them — `vblanks` will read ~240 and `flips` less — or the chain of
/// flip-vblank-flip is itself stalling, in which case `vblanks` reads ~204
/// too, and `empty` and `restarts` say what broke it. DRM only sends a vblank
/// event for a flip that was submitted, so a frame with nothing to draw ends
/// the chain until something restarts it, and the only thing that restarts it
/// is the frame clock — whose period is the refresh interval plus however
/// long its own tick took.
///
/// Off unless asked for. The counters themselves are a handful of increments,
/// but the summary is a log line a second, and a benchmark should not measure
/// a compositor that is writing about itself.
#[derive(Debug, Default)]
pub struct FrameLog {
    /// Vblank events that arrived.
    pub vblanks: u32,
    /// Flips queued. One per vblank is a chain that is keeping up.
    pub flips: u32,
    /// Render passes that reached the renderer and found nothing changed.
    /// Each one ends the chain: no flip, so no vblank to drive the next.
    pub empty: u32,
    /// Empty passes that ran with a client commit applied since the last flip.
    ///
    /// The half of `empty` that is not a still desk. An empty pass withholds
    /// the one signal a client pacing on presentation feedback waits for: no
    /// flip means no vblank, so no `presented`.
    ///
    /// Per device rather than per output, so on a multi-screen desk a commit
    /// on one screen counts against an empty pass on the other. Read with that
    /// in mind.
    pub empty_after_commit: u32,
    /// How long a client commit waits before the flip carrying it is queued,
    /// summed, and the worst single wait.
    ///
    /// The queueing delay, not the drawing cost — `render_nanos` already says
    /// the drawing is under a percent of a core. Note what it cannot see: a
    /// commit held by a fifo or commit-timing blocker has not reached the
    /// commit handler yet, so the wait starts when the blocker clears.
    pub commit_to_flip_nanos: u64,
    pub commit_to_flip_count: u32,
    pub commit_to_flip_worst: std::time::Duration,
    /// Render attempts that returned before building a frame because one was
    /// already in the air. Expected to be large — it is every commit a client
    /// makes between two frames — and cheap.
    pub skipped_pending: u32,
    /// Render attempts that returned because the output was off.
    pub skipped_off: u32,
    /// Renders driven by the frame clock rather than by a vblank. This is the
    /// chain being restarted, and the drift enters here.
    pub restarts: u32,
    /// Client surface commits. Equal to the client's frame rate by
    /// definition — one commit per frame it finishes — and here as the thing
    /// the two counters below are read against.
    pub commits: u32,
    /// Windows that had a frame callback queued when an invitation went out.
    ///
    /// This is the discriminator, and it is the number I should have gone
    /// looking for three theories ago. A client asks for a callback when it
    /// commits, and paints when it arrives. So at each frame:
    ///
    ///   `wanted` ≈ vblanks  — the client is asking every frame and being
    ///                         answered; if it still only paints 5 times in 6
    ///                         the invitation path is innocent and what is
    ///                         holding it up is on the buffer side, most
    ///                         likely waiting on one the compositor has not
    ///                         released.
    ///   `wanted` ≈ commits  — the client is *not* asking at some frames,
    ///                         because it has not committed yet, and the
    ///                         question moves to what it is blocked on.
    ///
    /// Either way this stops the guessing: every counter above measures the
    /// compositor's own frames, and all of them have read perfect while the
    /// client sat at five sixths of the refresh rate.
    pub wanted: u32,
    /// Fifo barriers signalled. This is the invitation for a client pacing
    /// itself with `wp_fifo_v1`, which is every client painting through Mesa —
    /// vkcube among them, which asks for no frame callbacks at all.
    ///
    /// Read against `flips`: a barrier per flip is a client being let go once
    /// per frame shown, which is the whole of what fifo promises.
    pub barriers: u32,
    /// Turns of the barrier tick — the timer that releases barriers when no
    /// vblank is coming. Like the frame clock it is armed after its own work,
    /// so it runs slower than the screen; unlike the frame clock it has never
    /// been measured. If barriers are being released from here rather than
    /// from the vblank while a client is painting, that is the same drift in
    /// a second place.
    pub barrier_ticks: u32,
    /// Turns of the barrier tick that did nothing because every screen had
    /// just flipped, and so left the job to those vblanks.
    pub barrier_ticks_deferred: u32,
    /// Turns that deferred while some screen had *not* flipped on its own.
    ///
    /// The multi-screen bug in one number, kept as a regression check. The
    /// deferral used to be one question for the device — "has anything flipped
    /// recently" — while the release it defers to walks a single screen's
    /// windows, so a monitor animating at the refresh rate silenced the tick
    /// for a second screen it never visited. Measured on a two-screen desk
    /// before the fix: 238 turns a second, every one deferred, every one of
    /// those on behalf of a screen that had not flipped.
    ///
    /// Now that the gate is asked per screen this should stay at zero. If it
    /// climbs, the gate has gone back to speaking for screens it does not
    /// visit.
    pub barrier_ticks_starved: u32,
    /// Turns of the event loop, and where the two things done at the end of
    /// each one go.
    ///
    /// calloop's own dispatch is not measurable from inside the callback, so
    /// what these bound is everything else: whatever is left after the commit
    /// handler, the drawing and the flush is calloop waking up and
    /// wayland-server parsing what arrived.
    pub loop_turns: u32,
    pub render_nanos: u64,
    pub flush_nanos: u64,
    /// Every client request, parsed and handed to a handler. The commit
    /// handler is inside this; so is everything vkcube sends around it —
    /// attach, damage, the fifo barrier, the commit-timing deadline.
    pub protocol_nanos: u64,
    pub protocol_dispatches: u32,
    /// Client requests actually parsed and handed to a handler.
    /// `dispatch_clients` counts them, which turns a percentage of a core into
    /// a price per request — the number that says whether the cost is the
    /// protocol implementation or what this compositor does inside it.
    pub protocol_messages: u64,
    /// CPU spent inside `EventLoop::dispatch` — everything calloop does per
    /// turn, including the client-request dispatch counted above.
    ///
    /// Thread CPU time, not wall clock, so blocking in `epoll_wait` is not in
    /// it. Subtract `protocol_nanos` and what is left is calloop's own work:
    /// the poll, deciding which sources are ready, and calling them.
    pub dispatch_cpu_nanos: u64,
    /// Thread CPU at the end of the last turn's callback, to difference
    /// against.
    pub cpu_at_turn: u64,
    /// Voluntary context switches at the last report: how many times this
    /// process actually blocked. Read against `loop_turns` it says whether a
    /// turn is a real wakeup or a spin — the loop going round again without
    /// ever sleeping, which is a different bug with a different fix.
    pub blocked_at: u64,
    /// The shell waking the compositor to collect a frame or a message.
    pub shell_pings: u32,
    /// Nanoseconds spent inside the surface commit handler.
    ///
    /// A client in IMMEDIATE mode commits as fast as buffers come back —
    /// thirteen thousand times a second — and costs this compositor 58
    /// microseconds of CPU per commit against sway's 15, for the same client
    /// throughput. Measuring the handler separates "the work done per commit
    /// is expensive" from "being woken thirteen thousand times a second is
    /// expensive", which are different bugs with different fixes and are
    /// indistinguishable from the outside.
    pub commit_nanos: u64,
    /// Vblanks that found no barrier to release.
    ///
    /// Ten a second on a sixty hertz screen, and they are the missing frames.
    /// What this cannot say on its own is why: either the client had not
    /// committed yet, or it had and something released the barrier before this
    /// vblank could — which is what `barriers_at_tick` is for.
    pub vblanks_without_barrier: u32,
    /// Barriers released by the tick rather than by a vblank.
    ///
    /// The tick exists for a desk where nothing is being submitted, so under a
    /// client painting flat out this should be nearly zero. If it is not, the
    /// two are racing for the same job and the drifting one is winning some of
    /// the time — which would put the client's rate on the tick's period
    /// rather than the screen's, and the tick is armed after its own work.
    pub barriers_at_tick: u32,
    /// The longest gap between consecutive vblanks in this window.
    pub worst_gap: std::time::Duration,
    /// Gaps longer than one and a half refresh periods: a frame period that
    /// went by with nothing on it.
    pub stalls: u32,
    /// When this window started, and when the last vblank was.
    pub since: Option<std::time::Instant>,
    pub last_vblank: Option<std::time::Instant>,
}

/// How many times this process has blocked, from the kernel's own count.
/// This thread's CPU time in nanoseconds.
pub fn thread_cpu_nanos() -> u64 {
    let now = smithay::reexports::rustix::time::clock_gettime(
        smithay::reexports::rustix::time::ClockId::ThreadCPUTime,
    );
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

fn blocked_count() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
                .and_then(|value| value.trim().parse().ok())
        })
        .unwrap_or(0)
}

impl FrameLog {
    /// Enabled by `VIEWPORT_FRAME_LOG`, which is read once.
    pub fn from_env() -> Option<Self> {
        std::env::var_os("VIEWPORT_FRAME_LOG").map(|_| Self::default())
    }

    /// One line a second, then start again.
    ///
    /// Rates rather than totals: "204 vblanks, 204 flips" against a 240Hz
    /// screen is the whole answer, and it is only legible per second.
    fn report_if_due(&mut self, refresh: std::time::Duration) {
        let now = std::time::Instant::now();
        let since = *self.since.get_or_insert(now);
        let elapsed = now.duration_since(since);
        if elapsed < std::time::Duration::from_secs(1) {
            return;
        }
        let per_second = |count: u32| f64::from(count) / elapsed.as_secs_f64();
        let expected = if refresh.is_zero() {
            0.0
        } else {
            1.0 / refresh.as_secs_f64()
        };
        tracing::info!(
            "frames: {:.1} vblanks/s of {:.1} possible, {:.1} flips/s, {:.1} client commits/s, \
             {:.1} windows waiting on a callback/s, {:.1} fifo barriers released/s \
             ({:.1} of them by the tick), {:.1} vblanks with no barrier/s, \
             {:.1} barrier ticks/s ({:.1} deferred to a vblank, {:.1} starved), \
             {:.1} empty/s ({:.1} after a commit), \
             commit to flip {:.2}ms each over {:.0}/s (worst {:.2}ms), \
             {} stalls (worst gap {:.2}ms), {:.1} clock restarts/s, \
             {:.0} skipped for a flip in the air, {:.0} skipped for an output that is off, \
             commit handler {:.1}us each and {:.0}% of a core, \
             {:.0} loop turns/s ({:.1} per commit), render {:.0}% of a core, flush {:.0}%, \
             calloop dispatch {:.0}% of a core ({:.2}us a turn, of which {:.2}us is calloop\'s own), \
             protocol {:.0}% of a core over {:.0} dispatches/s \
             ({:.0} requests/s, {:.1} per dispatch, {:.2}us each), {:.0} blocks/s, \
             {:.0} shell pings/s",
            per_second(self.vblanks),
            expected,
            per_second(self.flips),
            per_second(self.commits),
            per_second(self.wanted),
            per_second(self.barriers),
            per_second(self.barriers_at_tick),
            per_second(self.vblanks_without_barrier),
            per_second(self.barrier_ticks),
            per_second(self.barrier_ticks_deferred),
            per_second(self.barrier_ticks_starved),
            per_second(self.empty),
            per_second(self.empty_after_commit),
            if self.commit_to_flip_count > 0 {
                self.commit_to_flip_nanos as f64 / f64::from(self.commit_to_flip_count) / 1e6
            } else {
                0.0
            },
            per_second(self.commit_to_flip_count),
            self.commit_to_flip_worst.as_secs_f64() * 1000.0,
            self.stalls,
            self.worst_gap.as_secs_f64() * 1000.0,
            per_second(self.restarts),
            per_second(self.skipped_pending),
            per_second(self.skipped_off),
            if self.commits > 0 {
                self.commit_nanos as f64 / f64::from(self.commits) / 1_000.0
            } else {
                0.0
            },
            self.commit_nanos as f64 / elapsed.as_secs_f64() / 10_000_000.0,
            per_second(self.loop_turns),
            if self.commits > 0 {
                f64::from(self.loop_turns) / f64::from(self.commits)
            } else {
                0.0
            },
            self.render_nanos as f64 / elapsed.as_secs_f64() / 10_000_000.0,
            self.flush_nanos as f64 / elapsed.as_secs_f64() / 10_000_000.0,
            self.dispatch_cpu_nanos as f64 / elapsed.as_secs_f64() / 10_000_000.0,
            if self.loop_turns > 0 {
                self.dispatch_cpu_nanos as f64 / f64::from(self.loop_turns) / 1000.0
            } else {
                0.0
            },
            if self.loop_turns > 0 {
                self.dispatch_cpu_nanos.saturating_sub(self.protocol_nanos) as f64
                    / f64::from(self.loop_turns)
                    / 1000.0
            } else {
                0.0
            },
            self.protocol_nanos as f64 / elapsed.as_secs_f64() / 10_000_000.0,
            per_second(self.protocol_dispatches),
            self.protocol_messages as f64 / elapsed.as_secs_f64(),
            if self.protocol_dispatches > 0 {
                self.protocol_messages as f64 / f64::from(self.protocol_dispatches)
            } else {
                0.0
            },
            if self.protocol_messages > 0 {
                self.protocol_nanos as f64 / self.protocol_messages as f64 / 1000.0
            } else {
                0.0
            },
            {
                let now = blocked_count();
                let delta = now.saturating_sub(self.blocked_at);
                self.blocked_at = now;
                delta as f64 / elapsed.as_secs_f64()
            },
            per_second(self.shell_pings),
        );
        let last = self.last_vblank;
        let blocked = self.blocked_at;
        let cpu = self.cpu_at_turn;
        *self = Self::default();
        self.since = Some(now);
        self.last_vblank = last;
        self.blocked_at = blocked;
        self.cpu_at_turn = cpu;
    }
}

impl Udev {
    /// The GPU everything defaults to: where the shell allocates, what clients
    /// are told about, and the one a single-GPU machine has.
    pub fn primary(&self) -> &Device {
        &self.devices[0]
    }

    pub fn primary_mut(&mut self) -> &mut Device {
        &mut self.devices[0]
    }

    /// Every output on every GPU.
    pub fn outputs(&self) -> impl Iterator<Item = (OutputId, &Surface)> {
        self.devices.iter().enumerate().flat_map(|(device, gpu)| {
            gpu.surfaces.iter().map(move |(crtc, surface)| {
                (
                    OutputId {
                        device,
                        crtc: *crtc,
                    },
                    surface,
                )
            })
        })
    }

    pub fn surface(&self, id: OutputId) -> Option<&Surface> {
        self.devices.get(id.device)?.surfaces.get(&id.crtc)
    }

    pub fn surface_mut(&mut self, id: OutputId) -> Option<&mut Surface> {
        self.devices.get_mut(id.device)?.surfaces.get_mut(&id.crtc)
    }

    /// Just the surfaces, when which GPU they are on does not matter.
    pub fn surfaces(&self) -> impl Iterator<Item = &Surface> {
        self.devices.iter().flat_map(|gpu| gpu.surfaces.values())
    }

    pub fn surfaces_mut(&mut self) -> impl Iterator<Item = &mut Surface> {
        self.devices
            .iter_mut()
            .flat_map(|gpu| gpu.surfaces.values_mut())
    }

    /// Every output's id, for iterating without holding a borrow.
    pub fn ids(&self) -> Vec<OutputId> {
        self.outputs().map(|(id, _)| id).collect()
    }

    /// Find which output drives a given smithay `Output`.
    pub fn id_of(&self, output: &Output) -> Option<OutputId> {
        self.outputs()
            .find(|(_, surface)| surface.output == *output)
            .map(|(id, _)| id)
    }
}

/// Bring up the backend.
pub fn init(
    event_loop: &mut EventLoop<'static, ViewportState>,
    state: &mut ViewportState,
) -> Result<()> {
    let (session, notifier) = LibSeatSession::new().context("opening a libseat session")?;
    let seat = session.seat();

    // The GPU the seat says is primary, or the first one there is.
    //
    // Two nodes matter and they are not interchangeable. Modesetting happens
    // on the primary (card) node; the render node has no CRTCs and opening it
    // for KMS gets as far as a permission error that looks like a session
    // problem. Vulkan wants the render node, because that is the one that
    // needs no DRM master.
    // The GPU the seat calls primary, unless nothing can render for it.
    //
    // A virtual machine commonly has two: the firmware's VGA, which has a
    // display and no Vulkan, and a virtio-gpu, which has both. The seat calls
    // the first one primary, and picking it means scanning out on one device
    // while drawing on another — which on this machine got as far as a black
    // screen and a driver abort. So the candidates are ranked by whether a
    // Vulkan device actually exposes them, and only then by what the seat
    // thinks.
    //
    // Deduplicated, because `all_gpus` includes the primary and this puts the
    // primary in front of it — so the seat's primary was in the list twice,
    // always. The ranking below did not care, but the loop that opens the
    // *other* GPUs skips only the one that was chosen, so whenever the choice
    // was not the seat's primary that primary was opened twice. The second
    // open fails, and the warning for it says its outputs stay dark — about a
    // device that had been opened successfully a moment earlier and was
    // driving its monitors. Had it instead succeeded, the result would have
    // been two `Device`s for one card: two output managers, two renderers, and
    // every connector on it published twice.
    let candidates: Vec<DrmNode> = first_occurrences(
        primary_gpu(&seat)
            .ok()
            .flatten()
            .into_iter()
            .chain(all_gpus(&seat).unwrap_or_default())
            .filter_map(|path| DrmNode::from_path(path).ok())
            .filter_map(|node| match node.ty() {
                NodeType::Primary => Some(node),
                _ => node.node_with_type(NodeType::Primary)?.ok(),
            }),
    );
    if candidates.is_empty() {
        return Err(anyhow!("no GPU with a primary node found for seat {seat}"));
    }

    let renders_for = |card: &DrmNode| -> bool {
        let Ok(instance) = smithay::backend::vulkan::Instance::new(
            smithay::backend::vulkan::version::Version::VERSION_1_3,
            None,
        ) else {
            return false;
        };
        let render = card
            .node_with_type(NodeType::Render)
            .and_then(|node| node.ok())
            .unwrap_or(*card);
        VulkanDevice::for_node_exactly(&instance, &render).is_ok()
    };

    // Which GPU, when there is more than one and the ranking picks wrong.
    //
    // A hybrid laptop is the case: an integrated GPU driving the panel and a
    // discrete one that is faster, and the answer depends on whether you want
    // battery or frames — which is a preference, not something the compositor
    // can read off the hardware. Both are ranked identically here, so the
    // choice fell to whichever the seat happened to list first, and there was
    // no way to say otherwise.
    //
    // Matched on the device path, by substring, so `card1`, `renderD129` and a
    // full `/dev/dri/by-path/...` all work.
    let wanted = std::env::var("VIEWPORT_GPU").ok().filter(|s| !s.is_empty());
    let paths: Vec<Option<String>> = candidates
        .iter()
        .map(|card| card.dev_path().map(|p| p.to_string_lossy().into_owned()))
        .collect();
    let chosen = match gpu_named(&paths, wanted.as_deref()) {
        Choice::Ranked => None,
        Choice::Named(at) => Some(candidates[at]),
        Choice::NotFound => {
            // Named and not found: say which, and what there was to choose
            // from. Falling through in silence is how someone ends up
            // convinced the variable does nothing.
            tracing::warn!(
                "VIEWPORT_GPU={} matches no GPU on this seat; there is {}. \
                 Choosing as though it were unset.",
                wanted.as_deref().unwrap_or(""),
                paths
                    .iter()
                    .flatten()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            None
        }
    };

    let card = chosen
        .or_else(|| candidates.iter().find(|card| renders_for(card)).copied())
        .unwrap_or_else(|| {
            // Nothing matched. The fallback in `for_node` will draw on
            // whatever Vulkan device there is, which is right for a machine
            // with one GPU and a software renderer and wrong for nothing.
            tracing::warn!(
                "no GPU has a Vulkan device of its own; going with what the seat calls primary"
            );
            candidates[0]
        });

    // Always when there is a choice to be had, not only in passing: on a
    // machine where the wrong one was picked this line is the whole diagnosis,
    // and it names the variable that changes it.
    if candidates.len() > 1 {
        let listed: Vec<String> = candidates
            .iter()
            .filter_map(|c| c.dev_path())
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        tracing::info!(
            "{} GPUs ({}); drawing and scanning out on {card:?}{}",
            candidates.len(),
            listed.join(", "),
            if chosen.is_some() {
                " as VIEWPORT_GPU asked"
            } else {
                ". Set VIEWPORT_GPU to a device path to choose another"
            }
        );
    }

    // Same card, the node Vulkan should use — and the one clients are told to
    // allocate on, which is why a card without one is worth a word.
    let render = client_render_node(&card);

    tracing::info!("primary GPU: card {card:?}, render {render:?}");
    // Said out loud because it changes what the screen does, and a run whose
    // settings are not in its log cannot be compared with another one.
    tracing::info!(
        "scanout {}",
        if frame_flags().is_empty() {
            "off"
        } else {
            "on"
        }
    );

    let mut session = session;
    let (manager, mut renderer, gbm, drm_notifier) = open_device(&mut session, &card, &render)?;

    // Input.
    let mut libinput = smithay::reexports::input::Libinput::new_with_udev::<
        LibinputSessionInterface<LibSeatSession>,
    >(session.clone().into());
    libinput
        .udev_assign_seat(&seat)
        .map_err(|_| anyhow!("could not assign seat {seat} to libinput"))?;
    let input_backend = LibinputInputBackend::new(libinput.clone());

    event_loop
        .handle()
        .insert_source(input_backend, move |event, _, state| {
            // Device added and removed events carry no pointer or key, so the
            // generic handler ignores them; everything else routes as usual.
            if let InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. } = event {
                return;
            }
            state.process_input_event(event);
        })
        .map_err(|e| anyhow!("inserting the libinput source: {e}"))?;

    // VBlank: the frame is done, so the next one may start.
    let primary_vblank = event_loop
        .handle()
        .insert_source(drm_notifier, move |event, metadata, state| match event {
            DrmEvent::VBlank(crtc) => state.on_vblank(OutputId { device: 0, crtc }, metadata),
            DrmEvent::Error(error) => {
                tracing::error!("drm: {error}");
                // The driver telling us something went wrong on the device is
                // exactly what a GPU reset looks like from here, and the flip
                // that was outstanding when it happened is not coming back.
                state.note_gpu_error(0);
            }
        })
        .map_err(|e| anyhow!("inserting the drm source: {e}"))?;

    // Session: the VT was switched away from or back to.
    //
    // libinput has to be told, and not only DRM. logind revokes the evdev fds
    // along with the card's on a VT switch, and a context that was never
    // suspended does not reopen them when the session comes back: every device
    // stays dead, which is a desktop that draws but takes no key or click —
    // including the keybinds that would switch away again or quit.
    event_loop
        .handle()
        .insert_source(notifier, move |event, _, state| match event {
            SessionEvent::PauseSession => {
                libinput.suspend();
                state.on_session_paused();
            }
            SessionEvent::ActivateSession => {
                // A resume that fails is a session with no input at all, so it
                // is worth a line naming it; there is nothing to fall back to.
                if let Err(e) = libinput.resume() {
                    tracing::error!("resuming libinput: {e:?}");
                }
                state.on_session_resumed();
            }
        })
        .map_err(|e| anyhow!("inserting the session source: {e}"))?;

    // Connector hotplug, and whole-device hotplug.
    //
    // A GPU appearing and disappearing mid-session is not only someone pulling
    // a card out of an eGPU dock. It is what a driver does when a reset does not
    // take: amdgpu falls back to a bus reset, which unregisters the DRM device
    // and registers it again, and from here that is indistinguishable from an
    // unplug followed by a plug. Ignoring the pair, which is what this did, left
    // a compositor polling a revoked fd with every monitor on that card dark for
    // the rest of the session.
    let udev = UdevBackend::new(&seat).map_err(|e| anyhow!("udev: {e}"))?;
    event_loop
        .handle()
        .insert_source(udev, move |event, _, state| match event {
            UdevEvent::Changed { device_id } => {
                if DrmNode::from_dev_id(device_id).is_ok() {
                    state.on_connectors_changed();
                    // A monitor that comes back is a new output as far as the
                    // scan is concerned: mapped to the right of everything
                    // else, in connector-enumeration order, turned the way it
                    // left the factory. The arrangement it had, and then the
                    // config file, put it back.
                    state.restore_output_layout();
                    state.apply_output_config();
                }
            }
            UdevEvent::Added { device_id, path } => state.on_gpu_added(device_id, &path),
            UdevEvent::Removed { device_id } => state.on_gpu_removed(device_id),
        })
        .map_err(|e| anyhow!("inserting the udev source: {e}"))?;

    // The ping the shell uses to wake the loop, so a posted message is acted
    // on now rather than whenever unrelated input next arrives.
    #[cfg(feature = "wpe")]
    {
        let (ping, source) = smithay::reexports::calloop::ping::make_ping()
            .map_err(|e| anyhow!("creating the shell ping: {e}"))?;
        event_loop
            .handle()
            .insert_source(source, |_, _, state| {
                // Counted: this is the shell waking the compositor, and it is
                // one of the few sources that could plausibly fire thousands
                // of times a second.
                if let Some(log) = state.udev.as_mut().and_then(|u| u.frame_log.as_mut()) {
                    log.shell_pings += 1;
                }
                state.drain_shell()
            })
            .map_err(|e| anyhow!("inserting the shell ping: {e}"))?;
        state.shell_ping = Some(ping);
    }

    // The lease global, one per DRM device. Non-fatal if it cannot be made:
    // no client can lease, and everything else is unaffected.
    let lease_state = match smithay::wayland::drm_lease::DrmLeaseState::new::<ViewportState>(
        &state.display_handle,
        &card,
    ) {
        Ok(lease_state) => Some(lease_state),
        Err(e) => {
            tracing::warn!("no drm-lease global on {card:?}: {e}");
            None
        }
    };

    state.udev = Some(Udev {
        lease_state,
        leases: Vec::new(),
        session,
        // The GPU chosen above is device 0: what the shell allocates on, what
        // clients are told about, and on a single-GPU machine the only one.
        // Secondary GPUs are opened below, once this exists to hang them off.
        devices: vec![Device {
            node: card,
            render_node: render,
            feedback: device_feedback(&render, &mut renderer),
            renderer,
            gbm,
            manager,
            surfaces: HashMap::new(),
            online: true,
            bus: crate::recovery::bus_id(&card),
            vblank: Some(primary_vblank),
            recovery: crate::recovery::Step::Healthy,
            errors: 0,
            attempts: 0,
            offline_since: None,
            stepped_at: None,
            settle: 0,
        }],
        blanked: false,
        active: true,
        frame_log: FrameLog::from_env(),
        last_vblank: None,
        last_vblank_by_output: HashMap::new(),
        committed_since_flip: false,
        first_commit_at: None,
        pending_adds: Vec::new(),
    });
    if state
        .udev
        .as_ref()
        .is_some_and(|udev| udev.frame_log.is_some())
    {
        tracing::info!("VIEWPORT_FRAME_LOG is set: reporting frame pacing once a second");
    }

    // Every other GPU on the seat.
    //
    // Each gets its own renderer and its own output manager, and drives the
    // connectors wired to it. That is what makes a monitor on the second card
    // light up at all — before this the compositor opened one device and the
    // rest of the machine's outputs did not exist as far as it was concerned.
    //
    // Rendering happens on the GPU that scans out, rather than drawing
    // everything on the primary and copying across: a buffer is only cheap on
    // the device that allocated it. The cost is that a client buffer allocated
    // on the primary has to be importable by the secondary, which is what the
    // shared modifiers are for — where it cannot be, that surface does not
    // appear on that screen rather than the session failing.
    //
    // A GPU that cannot be opened is skipped with a warning. One card failing
    // is a monitor that stays dark; refusing to start is every monitor dark.
    for other in candidates.iter().filter(|c| **c != card) {
        let other_render = client_render_node(other);

        let Some(udev) = state.udev.as_mut() else {
            break;
        };
        match open_device(&mut udev.session, other, &other_render) {
            Ok((manager, mut renderer, gbm, notifier)) => {
                let index = udev.devices.len();
                udev.devices.push(Device {
                    node: *other,
                    render_node: other_render,
                    feedback: device_feedback(&other_render, &mut renderer),
                    renderer,
                    gbm,
                    manager,
                    surfaces: HashMap::new(),
                    online: true,
                    bus: crate::recovery::bus_id(other),
                    vblank: None,
                    recovery: crate::recovery::Step::Healthy,
                    errors: 0,
                    attempts: 0,
                    offline_since: None,
                    stepped_at: None,
                    settle: 0,
                });
                tracing::info!("gpu {index}: {other:?} also driving outputs");

                // Its vblanks carry its own index, because a crtc handle only
                // means something on the device that issued it.
                match event_loop.handle().insert_source(
                    notifier,
                    move |event, metadata, state: &mut ViewportState| match event {
                        DrmEvent::VBlank(crtc) => state.on_vblank(
                            OutputId {
                                device: index,
                                crtc,
                            },
                            metadata,
                        ),
                        DrmEvent::Error(e) => {
                            tracing::error!("drm error on gpu {index}: {e}");
                            state.note_gpu_error(index);
                        }
                    },
                ) {
                    Ok(token) => udev.devices[index].vblank = Some(token),
                    Err(e) => tracing::warn!(
                        "gpu {index}: no vblank source ({e}); its outputs will not draw"
                    ),
                }
            }
            Err(e) => {
                tracing::warn!("{other:?} could not be opened ({e:#}); its outputs stay dark");
            }
        }
    }

    if let Some(udev) = state.udev.as_ref() {
        if udev.devices.len() > 1 {
            let summary: Vec<String> = udev
                .devices
                .iter()
                .enumerate()
                .map(|(i, gpu)| {
                    format!(
                        "{i}: {} (render {})",
                        gpu.node
                            .dev_path()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_else(|| format!("{:?}", gpu.node)),
                        gpu.render_node
                            .dev_path()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_else(|| format!("{:?}", gpu.render_node)),
                    )
                })
                .collect();
            tracing::info!(
                "driving {} GPUs — {}",
                udev.devices.len(),
                summary.join(", ")
            );
        }
    }

    // Claim DRM master before anything is committed.
    //
    // The session is already active when the compositor starts on a TTY, so
    // no ActivateSession event ever arrives and nothing else would take it.
    // Without this the initial modeset appears to work and every page flip
    // afterwards fails with EPERM — which presents as a screen that stops
    // updating rather than as anything anyone would connect to permissions.
    //
    // Claimed on every GPU, not only the first. The reason above applies to
    // each of them — a secondary card is opened the same way and its session
    // is active the same way — so claiming it for one left every other card's
    // page flips failing with EPERM. That is the same "screen that stops
    // updating" this exists to prevent, on the monitors this compositor grew
    // multi-GPU support in order to light up at all.
    if let Some(udev) = state.udev.as_mut() {
        for index in 0..udev.devices.len() {
            if let Err(e) = udev.devices[index].manager.lock().activate(false) {
                tracing::error!("could not claim drm master on gpu {index}: {e}");
            } else {
                tracing::info!("gpu {index}: drm master claimed");
            }
        }
    }

    // Now that there is a renderer, clients can be told which formats they may
    // allocate — and on which GPU.
    {
        let formats = state
            .udev
            .as_ref()
            .map(|udev| {
                udev.primary()
                    .renderer
                    .dmabuf_formats()
                    .iter()
                    .copied()
                    .collect()
            })
            .unwrap_or_default();
        state.advertise_dmabuf(Some(render.dev_id()), formats);
    }

    state.on_connectors_changed();

    // Now that the outputs exist, the config file can say what they should be.
    // The restore is what notes down where they started, so a later unplug has
    // something to come back to even on a system whose config names no
    // positions at all.
    state.restore_output_layout();
    state.apply_output_config();
    if state.adaptive_sync {
        state.set_adaptive_sync(true);
    }

    // What a capture client may allocate on: the render node, not the card.
    // Allocation happens there, and it is the node a client may open without
    // being the session's master.
    {
        // Colour only. A capture buffer is copied *into*, and the importable
        // set now carries the YUV formats a video decoder produces — which can
        // be sampled and not written. Offering one would let a client allocate
        // a buffer every capture then fails against.
        let formats: Vec<_> = state
            .udev
            .as_ref()
            .map(|udev| {
                udev.primary()
                    .renderer
                    .dmabuf_formats()
                    .iter()
                    .copied()
                    .filter(|format| !viewport_vulkan::format::is_yuv(format.code))
                    .collect()
            })
            .unwrap_or_default();
        state.capture_gpu = render
            .node_with_type(NodeType::Render)
            .and_then(|node| node.ok())
            .or(Some(render))
            .map(|node| (node, formats));
    }

    // Explicit synchronisation, if this GPU can do it.
    //
    // Without it a client hands over a buffer and the compositor has to guess
    // when the GPU has finished writing it, which the kernel does through
    // implicit fences on the buffer itself. Vulkan clients have no implicit
    // fence to attach, and nvidia's driver has never had them at all — this is
    // the protocol that replaces the guess with the client saying so.
    //
    // Only advertised where the driver supports the eventfd form: the protocol
    // has no way to say "actually, no", so a global on a device that cannot do
    // it is a client waiting for a signal that never arrives.
    {
        let import_device = state
            .udev
            .as_ref()
            .map(|udev| udev.primary().manager.device().device_fd().clone());
        if let Some(import_device) = import_device {
            if smithay::wayland::drm_syncobj::supports_syncobj_eventfd(&import_device) {
                state.syncobj_state = Some(smithay::wayland::drm_syncobj::DrmSyncobjState::new::<
                    ViewportState,
                >(&state.display_handle, import_device));
                tracing::info!("explicit sync is available on this gpu");
            } else {
                tracing::info!("this gpu has no syncobj eventfd; implicit sync only");
            }
        }
    }

    #[cfg(feature = "wpe")]
    // Sizes, maps and focuses the view itself — WebKit paints nothing into an
    // unmapped view of no size.
    if state.shell_backend == crate::shell_backend::ShellBackend::Wpe {
        state.start_shell(&card, &render)?;
    }
    // The out-of-process shell needs none of this device: it allocates against
    // whatever GPU it opens for itself and hands the buffer over as a client.
    state.start_shell_process();
    // And the wallpaper terminal, if one was asked for. Beside the shell
    // because it needs the same thing the shell does: outputs, so it can be
    // told how big the desktop is.
    state.start_background_process();

    Ok(())
}

/// Open the DRM device and build everything that hangs off it.
pub fn open_device(
    session: &mut LibSeatSession,
    card: &DrmNode,
    render: &DrmNode,
) -> Result<(
    Manager,
    Gpu,
    GbmDevice<DrmDeviceFd>,
    smithay::backend::drm::DrmDeviceNotifier,
)> {
    let path = card
        .dev_path()
        .ok_or_else(|| anyhow!("{card:?} has no device path"))?;

    // Through the session rather than open(2): that is what makes this work
    // without being root, and what lets the fd be revoked on VT switch.
    let fd = session
        .open(
            &path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .map_err(|e| anyhow!("opening {}: {e}", path.display()))?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));

    // Atomic modesetting. The legacy path cannot express a commit that changes
    // several planes at once, which is the whole point of using DrmCompositor.
    let (drm, notifier) = DrmDevice::new(fd.clone(), true).context("creating the drm device")?;
    let gbm = GbmDevice::new(fd).context("creating the gbm device")?;

    let renderer = build_renderer(&gbm, render)?;
    let render_formats = renderer.dmabuf_formats();

    // SCANOUT as well as RENDERING: these buffers go to the display
    // controller, and a buffer allocated without it may not be scannable.
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );

    // The exporter turns a rendered buffer into a DRM framebuffer handle. It
    // takes the node so it can tell a buffer allocated here from one imported
    // from another GPU.
    let exporter = GbmFramebufferExporter::new(gbm.clone(), (*render).into());
    let gbm_kept = gbm.clone();

    let manager = DrmOutputManager::new(
        drm,
        allocator,
        exporter,
        Some(gbm),
        scanout_formats().iter().copied(),
        render_formats,
    );

    Ok((manager, renderer, gbm_kept, notifier))
}

/// A renderer for one GPU, on a GBM device that is already open.
///
/// Split out from `open_device` because it is the one part of a device that has
/// to be replaceable on its own. A Vulkan device that takes
/// `VK_ERROR_DEVICE_LOST` — which is what a GPU reset does to it — fails every
/// submission from then on and cannot be revived, while the GBM device it was
/// built on is still perfectly good. Rebuilding just this is the difference
/// between a reset costing a frame and costing the session. See
/// [`crate::recovery::Step::Rebuild`].
pub fn build_renderer(gbm: &GbmDevice<DrmDeviceFd>, render: &DrmNode) -> Result<Gpu> {
    // The Vulkan device has to be the same GPU, or every imported buffer is a
    // copy over PCIe. Selecting by node is what viewport_vulkan::open does.
    let instance = smithay::backend::vulkan::Instance::new(
        smithay::backend::vulkan::version::Version::VERSION_1_3,
        None,
    )
    .map_err(|e| {
        // The bare message is "Failed to load the Vulkan library", which names
        // the symptom and not the cause. It is almost always the library path:
        // the binary dlopens libvulkan rather than linking it, so it needs the
        // dev shell at run time and not only at build time.
        anyhow!(
            "{e}\n\
             \n\
             libvulkan.so.1 could not be loaded. It is dlopened, not linked, so \n\
             it has to be on the library path when the compositor runs:\n\
             \n\
             \x20   ./scripts/run-drm.sh\n\
             \n\
             which re-enters the dev shell for you. Running the binary directly \n\
             from a plain shell, or under sudo (which strips LD_LIBRARY_PATH), \n\
             gets exactly this error."
        )
    })?;

    // SCANOUT as well as RENDERING: these buffers go to the display
    // controller, and a buffer allocated without it may not be scannable.
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );

    // Vulkan unless it cannot serve this display, or unless told otherwise.
    //
    // VIEWPORT_RENDERER=gles forces the OpenGL path and =vulkan refuses to fall
    // back, which is what a bug report needs: "it works with the other one" is
    // the first thing worth knowing and should not require a rebuild.
    let asked = std::env::var("VIEWPORT_RENDERER").unwrap_or_default();
    let renderer = match asked.as_str() {
        // Through `renderer_forced_gles` rather than matching the names here,
        // because the shell's copy renderer asks the same question and the two
        // answering differently is a bug in its own right — see there.
        _ if renderer_forced_gles() => {
            tracing::info!("VIEWPORT_RENDERER={asked}: the OpenGL renderer");
            Gpu::Gles(Box::new(gles_renderer(gbm)?))
        }
        _ => {
            // `for_node_exactly`, not `for_node`. The loose version falls back
            // to whatever Vulkan device exists when none of them owns this
            // display's node, and in a virtual machine that is lavapipe — which
            // opens, builds a renderer, reports success, and then cannot
            // allocate anything the display controller will scan out:
            //
            //   Virtual-1: could not initialise: llvmpipe does not support
            //   DrmFourcc(AR24) with modifier 0xff
            //
            // Every output fails that way, the compositor comes up with no
            // outputs, nothing is ever committed, and the screen stays black
            // while the log says the renderer was created. GLES on this GPU's
            // own GBM device draws that same display without complaint, so a
            // Vulkan device that is not this GPU is worth less than OpenGL that
            // is.
            let vulkan = VulkanDevice::for_node_exactly(&instance, render)
                .context("opening a vulkan device on the primary GPU")
                .and_then(|device| {
                    VulkanRenderer::with_allocator(&device, allocator.clone())
                        .map_err(|e| anyhow!("creating the vulkan renderer: {e}"))
                });
            match vulkan {
                Ok(renderer) => Gpu::Vulkan(renderer),
                // Asked for Vulkan by name: any Vulkan device, including a
                // software one on another node, because that is what was asked
                // for and the alternative is refusing to start.
                Err(e) if asked == "vulkan" => {
                    tracing::warn!(
                        "VIEWPORT_RENDERER=vulkan: no Vulkan on this GPU ({e:#}); \
                         taking whatever device there is"
                    );
                    let device = VulkanDevice::for_node(&instance, render)
                        .context("opening any vulkan device")?;
                    Gpu::Vulkan(
                        VulkanRenderer::with_allocator(&device, allocator.clone())
                            .map_err(|e| anyhow!("creating the vulkan renderer: {e}"))?,
                    )
                }
                Err(e) => {
                    // A virtual machine is the usual reason, and OpenGL is the
                    // answer there rather than the software Vulkan device that
                    // is also on offer: virgl gives OpenGL ES on the GBM
                    // platform and draws the display, while lavapipe owns no
                    // DRM node and cannot allocate anything the display
                    // controller will take.
                    tracing::warn!("no usable Vulkan renderer ({e:#}); falling back to OpenGL");
                    match gles_renderer(gbm) {
                        Ok(gles) => Gpu::Gles(Box::new(gles)),
                        // Neither this GPU's Vulkan nor its OpenGL. Software
                        // Vulkan last, because it is the only thing left and a
                        // session that draws slowly beats one that does not
                        // start — the order being what matters: OpenGL on the
                        // real device before Vulkan on a pretend one.
                        Err(gles_error) => {
                            tracing::warn!(
                                "OpenGL would not start either ({gles_error:#});                                  taking whatever Vulkan device there is, which                                  is likely to be software"
                            );
                            let device = VulkanDevice::for_node(&instance, render)
                                .context("opening any vulkan device")?;
                            Gpu::Vulkan(
                                VulkanRenderer::with_allocator(&device, allocator.clone())
                                    .map_err(|e| anyhow!("creating the vulkan renderer: {e}"))?,
                            )
                        }
                    }
                }
            }
        }
    };

    Ok(renderer)
}

/// What a client should allocate against for a surface shown on this GPU.
///
/// The main device is this GPU's render node, and the formats are the ones its
/// renderer can actually import — which is the whole point on a machine with
/// more than one card. A client told the primary's node allocates something the
/// primary can import, and if the window is being displayed by another GPU it
/// is that one which has to import it.
///
/// No scanout tranche. That is the hint that lets a buffer go straight to a
/// plane, and claiming it for a format the display controller cannot actually
/// scan out is worse than not claiming it — it belongs with the plane formats,
/// which are per CRTC and not known here.
pub fn device_feedback(
    render_node: &DrmNode,
    renderer: &mut Gpu,
) -> Option<smithay::wayland::dmabuf::DmabufFeedback> {
    use smithay::backend::renderer::ImportDma as _;

    let formats: Vec<_> =
        crate::with_gpu!(renderer, |r| r.dmabuf_formats().iter().copied().collect());
    if formats.is_empty() {
        return None;
    }
    let node = render_node.dev_id();
    match smithay::wayland::dmabuf::DmabufFeedbackBuilder::new(node, formats).build() {
        Ok(feedback) => Some(feedback),
        Err(e) => {
            tracing::warn!("{render_node:?}: no per-surface dmabuf feedback ({e})");
            None
        }
    }
}

/// Feedback for one output: what to allocate, and what could reach a plane.
///
/// Two tranches. The main one names this GPU's render node and everything its
/// renderer can import, which is what makes a buffer usable at all. The
/// preferred one carries `Scanout` and the formats this output's primary plane
/// actually accepts, which is what lets a fullscreen buffer go straight to the
/// display controller instead of through a composite.
///
/// The scanout formats are intersected with what the renderer can import. A
/// format the plane takes and the compositor cannot read is no use: the moment
/// anything overlaps that surface it has to be composited, and a buffer that
/// cannot be sampled has nowhere to go. Advertising it would trade a rare
/// zero-copy frame for a window that vanishes when a notification appears over
/// it.
fn output_feedback(
    render_node: &DrmNode,
    device_node: &DrmNode,
    render_formats: &[smithay::backend::allocator::Format],
    plane_formats: &smithay::backend::allocator::format::FormatSet,
) -> Option<smithay::wayland::dmabuf::DmabufFeedback> {
    use smithay::reexports::wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1::TrancheFlags;

    if render_formats.is_empty() {
        return None;
    }
    let scanout: Vec<_> = plane_formats
        .iter()
        .filter(|format| render_formats.contains(format))
        .copied()
        .collect();

    let builder = smithay::wayland::dmabuf::DmabufFeedbackBuilder::new(
        render_node.dev_id(),
        render_formats.to_vec(),
    );
    let builder = if scanout.is_empty() {
        builder
    } else {
        builder.add_preference_tranche(
            device_node.dev_id(),
            TrancheFlags::Scanout,
            scanout,
            // Every protocol version: the tranche is useful to anything that
            // understands feedback at all.
            0..=u32::MAX,
        )
    };
    match builder.build() {
        Ok(feedback) => Some(feedback),
        Err(e) => {
            tracing::warn!("{device_node:?}: no per-output dmabuf feedback ({e})");
            None
        }
    }
}

/// Whether the session was told to draw with OpenGL.
///
/// Every renderer this compositor builds asks, not only the one that draws the
/// outputs. A session forced onto OpenGL that still copied the shell's frames
/// with Vulkan was two renderers disagreeing about one buffer: the copy was
/// allocated for the renderer that was asked for and imported by the one that
/// was not, and "it works with the other one" then answered nothing, because
/// the other one was never entirely out of the picture.
pub fn renderer_forced_gles() -> bool {
    matches!(
        std::env::var("VIEWPORT_RENDERER")
            .unwrap_or_default()
            .as_str(),
        "gles" | "gl" | "opengl"
    )
}

/// An OpenGL ES renderer on the same GBM device.
///
/// EGL rather than Vulkan, which is what makes it work where Vulkan cannot:
/// virgl in a guest exposes GL through the GBM platform, and this is the path
/// the nested backend has always used.
/// Generic over what the GBM device was opened on, because the shell's copy
/// renderer opens the render node itself — it needs no DRM master, so it holds
/// a plain `File` where the display path holds a session-managed `DrmDeviceFd`.
pub fn gles_renderer<A>(
    gbm: &GbmDevice<A>,
) -> Result<smithay::backend::renderer::gles::GlesRenderer>
where
    A: std::os::fd::AsFd + Clone + Send + 'static,
{
    let display = unsafe { smithay::backend::egl::EGLDisplay::new(gbm.clone()) }
        .context("opening an EGL display on the GBM device")?;
    let context =
        smithay::backend::egl::EGLContext::new(&display).context("creating an EGL context")?;
    // SAFETY: the context is current on this thread for the renderer's life,
    // which is the compositor's — the DRM path is single-threaded.
    unsafe { smithay::backend::renderer::gles::GlesRenderer::new(context) }
        .context("creating the OpenGL renderer")
}

impl ViewportState {
    /// Walk the connectors and bring up anything newly connected.
    /// Re-scan every GPU's connectors.
    ///
    /// Each device is scanned on its own: a connector belongs to the card it is
    /// wired to, and so does the CRTC that will drive it. Nothing is shared
    /// between them except the layout the outputs end up in.
    pub fn on_connectors_changed(&mut self) {
        let count = self
            .udev
            .as_ref()
            .map(|udev| udev.devices.len())
            .unwrap_or(0);
        for index in 0..count {
            self.scan_device(index);
        }
    }

    /// One GPU's connectors.
    fn scan_device(&mut self, index: usize) {
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        if !udev.active {
            // The fds are revoked while switched away; asking the device
            // anything here fails and the answer would be stale by the time we
            // came back anyway.
            return;
        }
        if index >= udev.devices.len() {
            return;
        }
        if !udev.devices[index].online {
            // The card is gone. Every ioctl below would answer ENODEV, and the
            // slot exists only so the index stays put until it comes back.
            return;
        }

        // Outputs brought up by this pass, so the first frame can be kicked
        // once the borrow on `udev` is released.
        let mut started: Vec<OutputId> = Vec::new();

        let device = udev.devices[index].manager.device();
        let Ok(resources) = device.resource_handles() else {
            tracing::error!("could not read drm resources");
            return;
        };

        let mut connectors: Vec<connector::Info> = resources
            .connectors()
            .iter()
            .filter_map(|handle| device.get_connector(*handle, true).ok())
            .filter(|info| info.state() == connector::State::Connected)
            .collect();
        // Stable sort by interface name and id, so outputs are placed left to
        // right in a deterministic order regardless of the kernel's enumeration.
        // A DisplayPort monitor that blanked drops and reconnects, and the DRM
        // driver may report its connectors in a different order afterwards —
        // which puts the same monitors in swapped positions.
        connectors.sort_by(|a, b| {
            a.interface()
                .as_str()
                .cmp(b.interface().as_str())
                .then(a.interface_id().cmp(&b.interface_id()))
        });

        // Worked out here, while the device is borrowed for reading, because
        // offering one for lease needs the lease state — which is a mutable
        // borrow of the same `udev`.
        let leasable: std::collections::HashSet<connector::Handle> = connectors
            .iter()
            .filter(|info| non_desktop(device, info.handle()))
            .map(|info| info.handle())
            .collect();

        tracing::info!("{} connected connector(s)", connectors.len());

        // Monitors that have gone since the last scan.
        //
        // This pass only ever added. It collects the connected connectors,
        // skips the ones that already have a surface, and builds the rest —
        // so unplugging a screen left its `Surface` in place, its `Output` in
        // the space, and its `wl_output` still advertised to every client. The
        // field holding that global says "dropped when the connector goes",
        // and nothing dropped it.
        //
        // What that costs: clients keep a screen in their output list that is
        // not there, the shell keeps laying windows out on it, and the render
        // loop keeps drawing frames for a connector that cannot show them. The
        // headless backend has removed outputs since it was written —
        // `output.test_remove` — and the hotplug test in control_socket.rs
        // exercises that path, which is why nothing caught this: the test is
        // headless and the bug is DRM's.
        //
        // Collected before anything is touched, because unmapping needs `self`
        // and the loop below is holding `udev`.
        let live: std::collections::HashSet<connector::Handle> =
            connectors.iter().map(|info| info.handle()).collect();
        let gone: Vec<(crtc::Handle, Output)> = udev.devices[index]
            .surfaces
            .iter()
            .filter(|(_, surface)| !live.contains(&surface.connector))
            .map(|(crtc, surface)| (*crtc, surface.output.clone()))
            .collect();

        // CRTCs already driving something *on this device*. Without this the
        // second monitor gets handed the first one's CRTC, and the "already in
        // use" check then drops it silently — which is exactly what happened
        // on the first two-monitor run.
        //
        // Scoped to the device, because a crtc handle is only meaningful on
        // the device that issued it — the same rule `on_vblank` is written
        // around. Taking the handles from every device and comparing them by
        // value alone means a CRTC busy on one GPU reserves whichever
        // unrelated CRTC happens to share its number on another, and a monitor
        // on the second card is refused with "no free crtc" while its own CRTCs
        // sit idle. The numbers collide readily: handles are small integers
        // handed out per device, so two cards almost always overlap.
        let mut taken: std::collections::HashSet<crtc::Handle> = udev
            .ids()
            .into_iter()
            .filter(|id| id.device == index)
            .map(|id| id.crtc)
            .collect();

        // Counted before the loop consumes them: the fallback below has to
        // tell "every connector failed" from "there were none".
        let attempted = connectors.len();
        for connector in connectors {
            let name = format!(
                "{}-{}",
                connector.interface().as_str(),
                connector.interface_id()
            );

            // A headset says so with the `non-desktop` property, and the right
            // thing to do with one is nothing: no output, no mode set, no
            // compositing. It is offered for lease instead, and a client that
            // knows how to drive it takes the whole connector.
            if leasable.contains(&connector.handle()) {
                if let Some(lease) = udev.lease_state.as_mut() {
                    tracing::info!("{name}: non-desktop, offered for lease");
                    lease.add_connector::<ViewportState>(
                        connector.handle(),
                        name.clone(),
                        format!("{name} (non-desktop)"),
                    );
                }
                continue;
            }

            // Already up, from an earlier pass.
            if udev.surfaces().any(|s| s.connector == connector.handle()) {
                continue;
            }

            // What the configuration asks for, then what the display prefers.
            //
            // A panel's preferred mode is often not its fastest: a 240Hz
            // monitor advertises 120Hz as preferred and then runs at half its
            // rate for anyone who never says otherwise. `"mode": "2560x1440@240"`
            // or `"max_refresh": true` is how the C build was told, and this is
            // where it is honoured.
            // By name, then by `*`. A configuration that says "every screen
            // as fast as it goes" should not have to name monitors it has not
            // met, and the C build took the wildcard — so a config written for
            // it was silently doing nothing here.
            let wanted = self
                .output_config
                .get(&name)
                .or_else(|| self.output_config.get("*"));
            let chosen =
                wanted.and_then(|config| crate::config::pick_mode(connector.modes(), config));
            if let Some(mode) = chosen.as_ref() {
                tracing::info!(
                    "{name}: {}x{}@{} from the configuration",
                    mode.size().0,
                    mode.size().1,
                    mode.vrefresh()
                );
            }
            let Some(mode) = chosen
                .as_ref()
                .or_else(|| {
                    connector
                        .modes()
                        .iter()
                        .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
                        .or_else(|| connector.modes().first())
                })
                .copied()
            else {
                continue;
            };

            let Some(crtc) =
                free_crtc(&udev.devices[index].manager, &resources, &connector, &taken)
            else {
                // Every CRTC this connector can reach is driving something
                // else. Real on hardware with more outputs than CRTCs, and
                // worth saying rather than skipping in silence.
                tracing::warn!("{name}: no free crtc");
                continue;
            };
            taken.insert(crtc);
            let (width, height) = connector.size().unwrap_or((0, 0));
            let output = Output::new(
                name.clone(),
                PhysicalProperties {
                    size: (width as i32, height as i32).into(),
                    subpixel: Subpixel::Unknown,
                    make: "Unknown".into(),
                    model: "Unknown".into(),
                    serial_number: "Unknown".into(),
                },
            );
            let global = output.create_global::<ViewportState>(&self.display_handle);
            let output_mode = OutputMode::from(mode);
            output.change_current_state(Some(output_mode), None, None, Some((0, 0).into()));
            output.set_preferred(output_mode);
            // Every mode the connector offers, not only the one being used. A
            // client configuring monitors can only choose from what it was
            // shown, and with one mode advertised the answer is always "the
            // resolution it already had".
            for candidate in connector.modes() {
                let candidate = OutputMode::from(*candidate);
                if candidate != output_mode {
                    output.add_mode(candidate);
                }
            }

            // initialize_output lives on the locked manager: bringing a
            // connector up touches every surface on the device, because
            // adding one can force the others onto different modifiers.
            // The manager is borrowed for the whole call, so the renderer
            // cannot come out of `udev` at the same time — it is taken and put
            // back around the two arms.
            let mut renderer =
                std::mem::replace(&mut udev.devices[index].renderer, Gpu::Placeholder);
            let result = crate::with_gpu!(&mut renderer, |r| {
                udev.devices[index]
                    .manager
                    .lock()
                    .initialize_output::<_, WaylandSurfaceRenderElement<_>>(
                        crtc,
                        mode,
                        &[connector.handle()],
                        &output,
                        None,
                        r,
                        &Default::default(),
                    )
                    .map_err(|e| e.to_string())
            });
            udev.devices[index].renderer = renderer;

            match result {
                Ok(drm_output) => {
                    // Side by side, left to right in the order the connectors
                    // are enumerated. Mapping every output at the origin
                    // instead stacks them, and the second monitor shows the
                    // first one's pixels.
                    //
                    // Real placement belongs to the shell, which sends
                    // output.configure once it is running; this is only a
                    // sane arrangement to start from.
                    let x = self
                        .space
                        .outputs()
                        .filter_map(|o| self.space.output_geometry(o))
                        .map(|geometry| geometry.loc.x + geometry.size.w)
                        .max()
                        .unwrap_or(0);

                    // The format is logged because it is chosen by
                    // negotiation: the preference order says what was asked
                    // for and only the surface says what the display agreed
                    // to, and "did the ten-bit request take" has no other
                    // answer short of dumping the planes.
                    let format = drm_output.with_compositor(|c| c.format());
                    tracing::info!(
                        "{name}: {}x{} at x={x}, {format:?}",
                        mode.size().0,
                        mode.size().1
                    );
                    // `map_output_at` inlined: `udev` is borrowed for the whole
                    // arm, so taking `&mut self` again here does not compile.
                    // Both halves still have to happen, or the second monitor
                    // tells clients it is at the origin like the first.
                    self.space.map_output(&output, (x, 0));
                    output.change_current_state(None, None, None, Some((x, 0).into()));
                    if self.active_output.is_none() {
                        self.active_output = Some(name);
                    }
                    // This output's own plane formats, for the scanout
                    // tranche. Read here because it is the only place the
                    // DrmOutput exists before it is handed over.
                    let plane_formats =
                        drm_output.with_compositor(|c| c.surface().plane_info().formats.clone());
                    let render_formats: Vec<_> =
                        crate::with_gpu!(&mut udev.devices[index].renderer, |r| {
                            smithay::backend::renderer::ImportDma::dmabuf_formats(r)
                                .iter()
                                .copied()
                                .collect()
                        });
                    let feedback = output_feedback(
                        &udev.devices[index].render_node,
                        &udev.devices[index].node,
                        &render_formats,
                        &plane_formats,
                    );

                    udev.devices[index].surfaces.insert(
                        crtc,
                        Surface {
                            output,
                            connector: connector.handle(),
                            drm_output,
                            feedback,
                            _global: global,
                            drawn: false,
                            dumped: false,
                            hdr: false,
                            powered: true,
                            tearing: false,
                            tearing_failures: 0,
                            refuses_tearing: false,
                            enabled: true,
                            pending: false,
                            queued_at: None,
                        },
                    );
                    // Device 0: this scan walks the primary GPU. When the
                    // other GPUs are scanned they push their own index.
                    started.push(OutputId {
                        device: index,
                        crtc,
                    });
                }
                Err(e) => tracing::warn!("{name}: could not initialise: {e}"),
            }
        }

        // A Vulkan device that cannot drive this display.
        //
        // Whether it can is not knowable until an output is attempted: the
        // renderer opens, reports its formats, and only `DrmCompositor` compares
        // them against what the plane will actually take. In a guest with Venus
        // that comparison fails on every connector —
        //
        //   Virtual-1: could not initialise: Virtio-GPU Venus (...) does not
        //   support DrmFourcc(AR24) with modifier 0xffffffffffffff
        //
        // `0x00ffffffffffffff` being `DRM_FORMAT_MOD_INVALID`, the implicit
        // modifier a virtio plane advertises — and the session comes up with no
        // outputs at all, which is a black screen and a log full of warnings
        // that each name one connector.
        //
        // OpenGL on this same GPU draws that display without complaint, so it
        // is taken rather than kept. Only when every connector failed and none
        // is left: one output refusing a mode is not this, and a machine with a
        // working Vulkan renderer never reaches here.
        if udev.devices[index].surfaces.is_empty()
            && attempted > 0
            && matches!(udev.devices[index].renderer, Gpu::Vulkan(_))
        {
            match gles_renderer(&udev.devices[index].gbm) {
                Ok(gles) => {
                    tracing::warn!(
                        "no output could be initialised with Vulkan on gpu {index}; \
                         drawing with OpenGL instead"
                    );
                    udev.devices[index].renderer = Gpu::Gles(Box::new(gles));
                    // Once: the renderer is OpenGL now, so this cannot arrive
                    // back here and swap again.
                    self.scan_device(index);
                    return;
                }
                Err(e) => tracing::error!(
                    "no output works with Vulkan on gpu {index} and OpenGL                      would not start either ({e:#}); this GPU has no display"
                ),
            }
        }

        self.notify_output_layout();
        // A monitor appearing or going is exactly what an output-management
        // client is watching for.
        self.advertise_outputs();

        // Draw once per new output. Rendering is driven by vblank, and vblank
        // only arrives after a frame has been queued — so without this first
        // push nothing ever draws and the compositor sits on a modeset screen
        // doing nothing, which is exactly what the first run on real hardware
        // did.
        for crtc in started {
            self.render(crtc);
        }

        // And drop the ones that went away, now that `udev` is no longer
        // borrowed and the space can be reached.
        //
        // Order matters against the loop above: a connector that vanished and
        // came back between two scans is not in `gone`, because `live` is read
        // from this same pass — so nothing here can remove an output the pass
        // just created.
        for (crtc, output) in gone {
            if let Some(udev) = self.udev.as_mut() {
                // Dropping the surface drops the `GlobalId` it holds, which is
                // what the field was there for.
                udev.devices[index].surfaces.remove(&crtc);
            }
            self.space.unmap_output(&output);

            // Whatever named this screen has to stop naming it.
            //
            // `active_output` is where a new window opens and what
            // `output.layout` reports as the active one. Nothing cleared it
            // when its monitor went, and `output_for_new_view` reads it before
            // the fallback that would have picked a live output — so unplugging
            // the active screen left every later window aimed at a name with
            // nothing behind it, and every `output.layout` reporting no active
            // output at all, because the name matched none of them.
            //
            // Moved to a screen that is still here rather than cleared, so the
            // answer stays a real one. The shell overrides it the moment focus
            // moves, which is the usual case; this is only for the gap.
            if self.active_output.as_deref() == Some(output.name().as_str()) {
                self.active_output = self.space.outputs().next().map(|o| o.name());
                tracing::info!(
                    "the active output was unplugged; now {}",
                    self.active_output.as_deref().unwrap_or("(none)")
                );
            }

            tracing::info!("{}: unplugged", output.name());
        }
        // The shell decides layout from the output list, and one screen fewer
        // is a different layout. Without this the windows stay where they were
        // — including on the monitor that is no longer there.
        self.notify_output_layout();
    }

    /// Everyone waiting to hear that this output's frame reached the screen.
    ///
    /// Walked from the elements that were actually drawn, so a surface handed
    /// straight to a plane is reported as scanned out rather than composited —
    /// which is the difference a client pacing itself by presentation cares
    /// about.
    fn presentation_feedback(
        &self,
        output: &smithay::output::Output,
        states: &smithay::backend::renderer::element::RenderElementStates,
    ) -> smithay::desktop::utils::OutputPresentationFeedback {
        use smithay::desktop::utils::{
            surface_presentation_feedback_flags_from_states, surface_primary_scanout_output,
        };

        // Which screen each surface was actually drawn on, written down before
        // anything asks.
        //
        // `surface_primary_scanout_output` reads state that nothing else in
        // this compositor writes, so without this it answers `None` for every
        // surface — and a surface that is on no output is left out of the
        // feedback entirely. The result is a compositor that advertises
        // `wp_presentation` and never once says a frame was presented.
        //
        // For a client that waits on frame callbacks that is invisible. For one
        // pacing itself on presentation it is fatal, and that is every client
        // that presents through Mesa: rio committed nine frames here and was
        // told about none of them, then stopped. Measured 2026-07-30 with
        // WAYLAND_DEBUG on a frozen terminal — nine commits, zero `presented`.
        self.update_scanout_outputs(output, states);
        self.send_dmabuf_feedback(output);

        let mut feedback = smithay::desktop::utils::OutputPresentationFeedback::new(output);
        for window in self.space.elements() {
            if self.space.outputs_for_element(window).contains(output) {
                window.take_presentation_feedback(
                    &mut feedback,
                    surface_primary_scanout_output,
                    |surface, _| {
                        surface_presentation_feedback_flags_from_states(surface, None, states)
                    },
                );
            }
        }
        for layer in smithay::desktop::layer_map_for_output(output).layers() {
            layer.take_presentation_feedback(
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
            );
        }
        // And the shell, which is in neither of those and had been getting no
        // feedback at all.
        //
        // The engines pace themselves differently and that is what made this
        // visible. WebKitGTK draws on frame callbacks, which the shell surface
        // has always been sent, and followed the panel at 120 and at 240Hz.
        // Chromium paces its compositor on presentation feedback, was told a
        // frame had been presented exactly never, and fell back to its own
        // 60Hz clock — which measured as Blink painting a fifth as often as
        // WebKit and read as the engine being slow. It was this.
        for surface in self.shell_client_surfaces() {
            smithay::desktop::utils::take_presentation_feedback_surface_tree(
                &surface,
                &mut feedback,
                // The output outright, not `surface_primary_scanout_output`.
                //
                // That helper answers from `RenderElementStates`, which is
                // keyed by render element id — and the shell's buffer is drawn
                // as a texture element under an id of the compositor's own,
                // not as a surface element. So it looks up the shell's surface,
                // finds nothing, and concludes the surface was not scanned out;
                // every feedback the shell asked for came back `discarded`.
                // Measured with WAYLAND_DEBUG: 723 requests, 723 discarded,
                // zero presented.
                //
                // Saying the output plainly is not a shortcut around the
                // check. The shell spans the whole layout and its buffer is
                // drawn on every output, every frame, underneath everything —
                // if this output presented at all, the shell was in it.
                |_, _| Some(output.clone()),
                |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
            );
        }
        // And the wallpaper. It is drawn as a surface tree, so unlike the shell
        // it has a real scan-out output recorded above and can be asked for it.
        for surface in self.background_surfaces() {
            smithay::desktop::utils::take_presentation_feedback_surface_tree(
                &surface,
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
            );
        }
        feedback
    }

    /// Tell each surface which GPU to allocate against.
    ///
    /// The `linux-dmabuf` global carries one default feedback naming the
    /// primary. That is the right answer while there is one GPU and the wrong
    /// one as soon as there are two: a window being displayed by the second
    /// card would be told to allocate for the first, and the renderer that has
    /// to import it belongs to the second. What follows is a surface that
    /// imports fine everywhere except the screen it is actually on.
    ///
    /// It matters for one GPU too. A client rendering on another device —
    /// anything under `prime-run`, or a discrete GPU on a hybrid machine —
    /// takes the main device from this feedback and allocates something that
    /// device can import. Getting it right is the difference between a
    /// zero-copy buffer and a rejected one the client has to allocate again.
    ///
    /// Sent per frame, from the same place the scan-out output is recorded,
    /// because `send_dmabuf_feedback` only tells a surface whose primary
    /// scan-out output is this one — which is state written immediately above.
    fn send_dmabuf_feedback(&self, output: &smithay::output::Output) {
        use smithay::desktop::utils::surface_primary_scanout_output;

        let Some(udev) = self.udev.as_ref() else {
            return;
        };
        // This output's own feedback where there is one, because only that
        // carries the scanout tranche — which formats could reach this
        // monitor's plane rather than being composited. The device's is the
        // fallback: right about which GPU to allocate against, silent about
        // scanout.
        let Some(id) = udev.id_of(output) else {
            return;
        };
        let Some(feedback) = udev
            .surface(id)
            .and_then(|surface| surface.feedback.as_ref())
            .or_else(|| {
                udev.devices
                    .get(id.device)
                    .and_then(|device| device.feedback.as_ref())
            })
        else {
            return;
        };

        for window in self.space.elements() {
            if self.space.outputs_for_element(window).contains(output) {
                window
                    .send_dmabuf_feedback(output, surface_primary_scanout_output, |_, _| feedback);
            }
        }
        for layer in smithay::desktop::layer_map_for_output(output).layers() {
            layer.send_dmabuf_feedback(output, surface_primary_scanout_output, |_, _| feedback);
        }
        // And the wallpaper, which allocates against a GPU like any other
        // client and was being told nothing about which one.
        for surface in self.background_surfaces() {
            smithay::desktop::utils::send_dmabuf_feedback_surface_tree(
                &surface,
                output,
                surface_primary_scanout_output,
                |_, _| feedback,
            );
        }
    }

    /// Record which output each surface was drawn on for this frame.
    ///
    /// Smithay keeps this per surface and offers it back through
    /// `surface_primary_scanout_output`, which presentation feedback, dmabuf
    /// feedback and frame-callback throttling all ask. Nothing wrote it, so all
    /// three were working from `None`.
    ///
    /// The comparison is Smithay's default: of the outputs a surface is on, the
    /// one showing the most of it wins, and a surface put straight on a plane
    /// beats a composited one. That is the answer a client pacing itself wants
    /// — "where is this actually being shown" — rather than "where might it be".
    fn update_scanout_outputs(
        &self,
        output: &smithay::output::Output,
        states: &smithay::backend::renderer::element::RenderElementStates,
    ) {
        use smithay::backend::renderer::element::default_primary_scanout_output_compare;
        use smithay::desktop::utils::update_surface_primary_scanout_output;

        for window in self.space.elements() {
            window.with_surfaces(|surface, surface_states| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    surface_states,
                    None,
                    states,
                    default_primary_scanout_output_compare,
                );
            });
        }
        for layer in smithay::desktop::layer_map_for_output(output).layers() {
            layer.with_surfaces(|surface, surface_states| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    surface_states,
                    None,
                    states,
                    default_primary_scanout_output_compare,
                );
            });
        }
        // The shell, for the same reason its feedback is taken above: a
        // surface with no primary scanout output recorded is dropped from the
        // feedback entirely, so this has to happen or that does nothing.
        for shell in self.shell_client_surfaces() {
            smithay::wayland::compositor::with_surface_tree_downward(
                &shell,
                (),
                |_, _, _| smithay::wayland::compositor::TraversalAction::DoChildren(()),
                |surface, surface_states, _| {
                    update_surface_primary_scanout_output(
                        surface,
                        output,
                        surface_states,
                        None,
                        states,
                        default_primary_scanout_output_compare,
                    );
                },
                |_, _, _| true,
            );
        }
        // And the wallpaper, which is drawn under all of it as a surface tree
        // of its own — in the space, in no layer map, so neither walk above
        // reaches it. A background terminal pacing on presentation was told
        // about no frame at all and stopped after the first.
        for surface in self.background_surfaces() {
            smithay::desktop::utils::with_surfaces_surface_tree(
                &surface,
                |surface, surface_states| {
                    update_surface_primary_scanout_output(
                        surface,
                        output,
                        surface_states,
                        None,
                        states,
                        default_primary_scanout_output_compare,
                    );
                },
            );
        }
        for lock in self.lock_surfaces.values() {
            smithay::desktop::utils::with_surfaces_surface_tree(
                lock.wl_surface(),
                |surface, surface_states| {
                    update_surface_primary_scanout_output(
                        surface,
                        output,
                        surface_states,
                        None,
                        states,
                        default_primary_scanout_output_compare,
                    );
                },
            );
        }
    }

    /// A frame finished scanning out, so the next one may be drawn.
    pub fn on_vblank(
        &mut self,
        id: OutputId,
        metadata: &mut Option<smithay::backend::drm::DrmEventMetadata>,
    ) {
        let _ = metadata;
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        // By device as well as crtc: a handle is only unique within the GPU
        // that issued it, so a vblank from the second would otherwise land on
        // an output of the first.
        let Some(surface) = udev.surface_mut(id) else {
            return;
        };
        // The flip happened, so this output may be drawn into again.
        surface.pending = false;
        // And the clock on it stops. A flip that reached the screen is the one
        // piece of evidence that this GPU is answering, which is what puts it
        // back at the top of the recovery ladder below.
        surface.queued_at = None;
        let vblank_refresh = surface
            .output
            .current_mode()
            .map(|mode| std::time::Duration::from_secs_f64(1_000.0 / mode.refresh.max(1) as f64))
            .unwrap_or_default();
        // Which screen this was, before the borrow on the surface is given up.
        let flipped = surface.output.name();
        // The chain is alive; the frame clock can stay out of the way.
        udev.last_vblank = Some(std::time::Instant::now());
        // And the same for this screen alone, which is what the barrier tick
        // asks. See `last_vblank_by_output`.
        udev.last_vblank_by_output
            .insert(flipped, std::time::Instant::now());
        // Whatever had committed has now been shown. An empty pass after this
        // point is a still desk until a client paints again.
        udev.committed_since_flip = false;

        // Between the two `surface_mut` calls on purpose: that borrow holds
        // all of `udev`, and the counters live in `udev` too. The surface is
        // taken again below rather than held across this.
        if let Some(log) = udev.frame_log.as_mut() {
            let now = std::time::Instant::now();
            log.vblanks += 1;
            if let Some(previous) = log.last_vblank.replace(now) {
                let gap = now.duration_since(previous);
                if gap > log.worst_gap {
                    log.worst_gap = gap;
                }
                // A gap of more than one and a half periods means a whole
                // frame period went by with nothing on it.
                if !vblank_refresh.is_zero() && gap > vblank_refresh.mul_f64(1.5) {
                    log.stalls += 1;
                }
            }
            log.report_if_due(vblank_refresh);
        }
        let Some(surface) = udev.surface_mut(id) else {
            return;
        };
        // A held attempt cannot outlive a frame. If its timer were ever lost —
        // dropped loop source, torn-down output — the flag alone would stop
        // this screen from ever drawing again, so the vblank clears it. An
        // extra timer that finds nothing to do is the harmless failure.
        // A vblank is the start of a new frame period, so the render this
        // vblank drives is never a repeat of the last one and must not be
        // held back. Throttling it costs the whole refresh — the flip misses
        // its window and the screen runs at half rate, which is a video that
        // judders rather than a compositor that idles.
        let refresh = surface
            .output
            .current_mode()
            .map(|mode| {
                // `refresh` is millihertz, so the period in seconds is
                // 1000/refresh. Dividing by a further thousand told every
                // client the screen ran at 240,000Hz, and a browser pacing
                // video on presentation feedback answered that by giving up
                // and falling back to a slow timer — a video at five frames a
                // second on a 240Hz panel, whenever nothing else was waking
                // the compositor.
                std::time::Duration::from_secs_f64(1_000.0 / mode.refresh.max(1) as f64)
            })
            .unwrap_or_else(|| std::time::Duration::from_millis(16));
        match surface.drm_output.frame_submitted() {
            // The frame is on the screen, and this is the moment the clients
            // that asked were waiting for. A compositor that advertises
            // wp_presentation and never answers leaves anything pacing itself
            // by presentation — a browser, most visibly — with no idea when
            // its last frame landed.
            Ok(Some(Some(mut feedback))) => {
                // A software clock read here, not `metadata.time`, and that is
                // not an oversight — it was tried.
                //
                // The reasoning for the hardware timestamp is sound on its
                // face: this reads the clock after the event has been queued,
                // delivered and dispatched, so it runs late by a varying
                // amount, and it is sent with `HwClock` set, which tells the
                // client it came from the display. Clients schedule from it.
                //
                // Measured, it makes things worse. Feeding `metadata.time`
                // through instead moved 240Hz from 198.0fps to 206.9 — and
                // took 60Hz from a steady 50.4 to 35.9 and then 43.2 across
                // two runs, with the compositor itself dropping to 42-48
                // vblanks a second and reporting 6-9 stalls where it had been
                // flipping on every vblank with none. Unstable, not just
                // slower, and instability is worse than the constant shortfall
                // it was meant to fix.
                //
                // So this stays until there is an explanation for that, rather
                // than a second guess at it. The `HwClock` flag below is
                // wrong — it is claiming a provenance this number does not
                // have — and that is worth fixing on its own, but it is not
                // worth fixing blind.
                let fallback = {
                    let now = smithay::reexports::rustix::time::clock_gettime(
                        smithay::reexports::rustix::time::ClockId::Monotonic,
                    );
                    std::time::Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
                };
                use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind;
                let (clock, sequence, flags) = match metadata.as_ref() {
                    Some(metadata) => match metadata.time {
                        smithay::backend::drm::DrmEventTime::Monotonic(_) => (
                            fallback,
                            metadata.sequence,
                            Kind::Vsync | Kind::HwClock | Kind::HwCompletion,
                        ),
                        _ => (fallback, metadata.sequence, Kind::Vsync),
                    },
                    None => (fallback, 0, Kind::Vsync),
                };
                feedback.presented::<_, smithay::utils::Monotonic>(
                    clock,
                    smithay::wayland::presentation::Refresh::Fixed(refresh),
                    sequence as u64,
                    flags,
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("frame_submitted: {e}"),
        }

        // A frame reached the screen, which is the one piece of evidence that
        // this GPU is answering. Whatever the watchdog was part-way through
        // recovering, it is finished with. See [`crate::recovery`].
        self.note_gpu_alive(id.device);

        // Anything blocked waiting for this frame: a client pacing itself with
        // wp-fifo, or timing a commit with wp-commit-timing.
        //
        // Here rather than in `render`, because this is where the frame
        // actually reached the screen — which is what those protocols are
        // about — and because releasing on every render attempt is a loop:
        // the release applies the commit it was holding, the commit marks the
        // output dirty, the render releases again. With a terminal that paces
        // itself that was nine thousand renders a second on a screen where
        // nothing was moving.
        let output = self
            .udev
            .as_ref()
            .and_then(|udev| udev.surface(id))
            .map(|surface| surface.output.clone());
        if let Some(output) = output {
            let at = self.start_time.elapsed();
            // Counted by the change in the barrier tally rather than by what
            // the call returns: it returns true for a commit-timing deadline
            // as well, and with one of those signalled on most frames this
            // read zero while ten vblanks a second were finding no *fifo*
            // barrier at all. A counter that agrees with the bug is worse than
            // no counter.
            let before = self
                .udev
                .as_ref()
                .and_then(|udev| udev.frame_log.as_ref())
                .map(|log| log.barriers);
            self.release_frame_barriers(&output, at);
            if let Some(before) = before {
                if let Some(log) = self.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
                    if log.barriers == before {
                        log.vblanks_without_barrier += 1;
                    }
                }
            }

            // And invite the clients on this output to draw the next one.
            //
            // This is the moment a frame reached the screen, so it is the
            // moment to ask for the frame after it: a client waiting on a
            // frame callback is paced by whatever sends it, and until this
            // existed the only thing that sent one was the frame clock.
            //
            // That clock is a timer re-armed after its own tick — callbacks,
            // render, flush, then `now + interval` — so its period is the
            // refresh interval plus however long that took. Always a little
            // slower than the screen, so a client that draws on every
            // invitation still misses a vblank now and then, and the shortfall
            // does not depend on the rate: 102.3fps of 120, and 204.1 of
            // 239.76, are both 85% of the vblanks on the floor. It was not a
            // capacity problem — the compositor was at 3.6% of a core and the
            // GPU at 3.8% while dropping them — it was asking late.
            //
            // The clock stays, because it is what restarts a desktop where
            // nothing is committing and so no vblank is coming. It is the
            // fallback now rather than the pacer.
            self.send_frame_callbacks(&output, at);
        }
        // And if anything is still waiting, keep a clock on it: a blocked
        // commit makes no damage, so without this nothing would draw again.
        self.arm_barrier_tick();

        self.render(id);
    }

    /// Draw one output.
    pub fn render(&mut self, id: OutputId) {
        let start = self.start_time.elapsed();

        let Some(output) = self
            .udev
            .as_ref()
            .and_then(|udev| udev.surface(id))
            .map(|surface| surface.output.clone())
        else {
            return;
        };

        // Nothing can come of this frame, so do not build it.
        //
        // These three are checked again in `render_pass`, which is where they
        // have always been — but everything between here and there ran first:
        // the shell's frame imported, and `frame_for` walking the layer map
        // and the space and cloning every element into a fresh list, all to be
        // dropped on the far side of the check.
        //
        // That is invisible while clients paint at the refresh rate and
        // ruinous when one does not. A client in IMMEDIATE or MAILBOX commits
        // as fast as buffers come back — thirteen thousand times a second —
        // and every one of those commits marks its output dirty, which drives
        // a full frame build that ended here. Against 120 flips a second that
        // is 13,600 frames built and thrown away, and it measured as four
        // times sway's CPU for the same client throughput.
        let skip = self
            .udev
            .as_ref()
            .and_then(|udev| udev.surface(id))
            .map(|surface| (!surface.powered || !surface.enabled, surface.pending));
        if let Some((off, pending)) = skip {
            if off || pending {
                if let Some(log) = self.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
                    if off {
                        log.skipped_off += 1;
                    } else {
                        log.skipped_pending += 1;
                    }
                }
                return;
            }
        }

        // Everything the frame needs, worked out before the renderer is
        // borrowed — and shared with the nested backend, which is what keeps
        // the two showing the same desktop.
        #[cfg(feature = "wpe")]
        self.import_shell_frame();
        let frame = self.frame_for(&output);
        let wants_tearing = self.output_wants_tearing(&output);
        let settled_for = self.last_layout.map(|at| at.elapsed());
        let mut pending_dump = false;

        let Some(mut udev) = self.udev.take() else {
            return;
        };
        // Out of the struct and onto the stack: the call below borrows the rest
        // of `udev` and all of `self`, which it cannot do while the renderer is
        // still a field of one of them.
        //
        // The renderer belonging to the GPU this output is on, not the
        // primary's. `render` is handed an `OutputId`, which names a device
        // precisely because the output need not be on the first one — and this
        // took `devices[0]` regardless. Every screen on a second card was
        // therefore drawn with the first card's renderer and then committed to
        // a `DrmOutput` whose allocator belongs to the second, which is the
        // cross-device copy the secondary-GPU path was written to avoid: "a
        // buffer is only cheap on the device that allocated it". Each
        // secondary built a renderer at open time that nothing ever used.
        let Some(primary_device) = udev.devices.get_mut(id.device) else {
            // The device index came out of an OutputId this compositor made,
            // so this cannot happen — but a panic in the render path would
            // take the session down for one stale id.
            tracing::error!("render: no gpu {} for {id:?}", id.device);
            self.udev = Some(udev);
            return;
        };
        let mut gpu = std::mem::replace(&mut primary_device.renderer, Gpu::Placeholder);
        // Colour management belongs to the Vulkan renderer; GLES draws in the
        // output's own space, so an HDR screen driven by it gets the ordinary
        // one — the honest result of that renderer not having the transforms.
        if let Gpu::Vulkan(renderer) = &mut gpu {
            let description = if udev.surface(id).map(|surface| surface.hdr).unwrap_or(false) {
                viewport_vulkan::color::Description {
                    primaries: viewport_vulkan::color::Primaries::BT2020,
                    transfer: viewport_vulkan::color::TransferFunction::Pq,
                    reference_luminance: 203.0,
                }
            } else {
                viewport_vulkan::color::Description::default()
            };
            renderer.set_output_description(description);
        }
        crate::with_gpu!(&mut gpu, |renderer| self.render_pass(
            &mut udev,
            renderer,
            id,
            &output,
            frame,
            wants_tearing,
            settled_for,
            start,
            &mut pending_dump,
        ));
        // Back to the device it came from, for the same reason.
        if let Some(device) = udev.devices.get_mut(id.device) {
            device.renderer = gpu;
        }
        self.udev = Some(udev);
    }

    /// One frame, with whichever renderer the device chose.
    ///
    /// Split out so the body is written once and compiled for both: the
    /// element list is typed by the renderer, so everything from building it
    /// to handing it to KMS has to live on this side of the choice.
    #[allow(clippy::too_many_arguments)]
    fn render_pass<R>(
        &mut self,
        udev: &mut Udev,
        renderer: &mut R,
        id: OutputId,
        output: &smithay::output::Output,
        frame: crate::render::Frame,
        wants_tearing: bool,
        settled_for: Option<std::time::Duration>,
        start: std::time::Duration,
        pending_dump: &mut bool,
    ) where
        R: smithay::backend::renderer::Renderer
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::Bind<smithay::backend::allocator::dmabuf::Dmabuf>
            + smithay::backend::renderer::ExportMem
            + smithay::backend::renderer::ImportDma
            + smithay::backend::renderer::Bind<<R as Captures>::Buffer>
            + smithay::backend::renderer::Offscreen<<R as Captures>::Buffer>
            + Captures,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        if !udev.active {
            return;
        }
        // Read before the surface takes a borrow of `udev` for the rest of
        // this. `Option<Instant>` is `Copy`, so this is the value and not a
        // handle on the device.
        let mut udev_commit_at = udev.first_commit_at;
        let mut flipped_at: Option<std::time::Instant> = None;
        let Some(surface) = udev.surface_mut(id) else {
            return;
        };
        if !surface.powered {
            // Asleep at a client's request. It keeps its place in the layout,
            // so there is a frame to draw when it wakes; there is just nowhere
            // to put it now.
            return;
        }
        if !surface.enabled {
            // Switched off by a client. Nothing to draw, and no vblank will
            // come to drive the next frame either.
            return;
        }
        // Already waiting on a flip. Drawing now would be overwritten before
        // it was ever scanned out; the request is remembered and the vblank
        // draws it.
        if surface.pending {
            // Nothing to remember. The flip in the air ends in a vblank, and
            // `on_vblank` draws this output again as its last act — so asking
            // for a frame here only means asking again immediately, and again,
            // for as long as the flip takes. That is a busy loop with a
            // compositor at one core and a log full of "nothing to draw"; it
            // was previously spread over every output by the global flag,
            // which made it look like the *other* monitor's problem.
            return;
        }

        // Tearing, if one window covers this output and asked for it. Set
        // before the frame is built, because it changes how the frame that is
        // about to be queued reaches the screen.
        let wants_tearing = wants_tearing && !surface.refuses_tearing;
        if surface.tearing != wants_tearing {
            surface.tearing = wants_tearing;
            let honoured = surface
                .drm_output
                .with_compositor(|compositor| compositor.set_allow_tearing(wants_tearing));
            tracing::info!(
                "{}: tearing {}{}",
                output.name(),
                if wants_tearing { "on" } else { "off" },
                if honoured {
                    ""
                } else {
                    " (this display cannot, so it will not)"
                }
            );
        }

        // The colour description this output expects is set in `render()`,
        // before the renderer is lent out. A second copy stood here and was
        // computed and dropped, which is worse than not having one: two
        // identical blocks where only one does anything is how the next colour
        // bug gets written.
        let elements = crate::render::build(&frame, renderer);

        // A composite of exactly this list, for when the screen and the log
        // disagree.
        if let Some(path) = crate::dump::output_target() {
            let settled = settled_for
                .map(|d| d >= std::time::Duration::from_secs(2))
                .unwrap_or(false);
            // Keep drawing until it fires: the capture needs a frame after the
            // layout has settled, and settling is precisely when nothing is
            // asking for one.
            if !surface.dumped && !frame.windows.is_empty() {
                *pending_dump = true;
            }
            if !surface.dumped && !frame.windows.is_empty() && settled {
                surface.dumped = true;
                let size = output
                    .current_mode()
                    .map(|m| output.current_transform().transform_size(m.size))
                    .unwrap_or_else(|| (0, 0).into());
                let path = path.with_file_name(format!(
                    "{}-{}.ppm",
                    path.file_stem().unwrap_or_default().to_string_lossy(),
                    output.name()
                ));
                tracing::info!("dumping {}: {} element(s)", output.name(), elements.len());
                if let Err(e) = crate::dump::output_frame::<_, <R as Captures>::Buffer, _>(
                    renderer,
                    &elements,
                    size,
                    [0.1, 0.1, 0.1, 1.0],
                    &path,
                ) {
                    tracing::error!("could not dump {}: {e:#}", output.name());
                }
            }
        }

        // Whether this call put a frame in the air, which decides whether the
        // clients on this output are invited to draw another one.
        let mut submitted = false;
        // Whether the device refused it. See the error arms below.
        let mut failed = false;
        let result = surface.drm_output.render_frame(
            renderer,
            &elements,
            // Behind everything, and behind the shell too — visible only where
            // nothing else covers it.
            [0.1, 0.1, 0.1, 1.0],
            // While tearing, only the primary plane may change: an
            // asynchronous flip is rejected outright if the commit touches
            // anything else, which is the EINVAL a game asking for tearing
            // produced. The cursor is composited into the frame instead of
            // riding its own plane, which is what it does on any frame where
            // the plane is unavailable anyway.
            if surface.tearing {
                FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT
            } else {
                frame_flags()
            },
        );

        match result {
            Ok(rendered) if !rendered.is_empty => {
                // Who asked to be told when this frame is on screen. Taken
                // from the very elements that were drawn, so a surface that
                // was scanned out directly is reported as such.
                // From the `udev` handed in: the caller took it out of `self`
                // so the renderer could be borrowed, and reaching for
                // `self.udev` here finds nothing and skips the flip — which is
                // a screen that never comes up at all.
                let feedback = self.presentation_feedback(output, &rendered.states);
                let Some(surface) = udev.surface_mut(id) else {
                    return;
                };
                if let Err(e) = surface.drm_output.queue_frame(Some(feedback)) {
                    // The vblank that would have driven the next frame never
                    // arrives, so nothing else will ask this output to draw —
                    // which is why the failure is counted rather than only
                    // logged. A single refused commit is a frame; a run of them
                    // is a GPU that has stopped, and the watchdog is what tells
                    // the two apart. See [`crate::recovery`].
                    tracing::warn!("queue_frame: {e}");
                    failed = true;

                    // A tearing flip is the one thing here a driver may refuse
                    // for a frame it would otherwise have taken — the
                    // capability says the hardware can flip asynchronously,
                    // not that it can do so in whatever state this output is
                    // in, and adaptive sync is the combination that goes wrong
                    // in practice. Giving the frame up and going back to whole
                    // ones costs a game its latency; not doing so costs the
                    // user their screen, because nothing will ask for another
                    // frame.
                    if surface.tearing {
                        surface.tearing = false;
                        let _ = surface
                            .drm_output
                            .with_compositor(|compositor| compositor.set_allow_tearing(false));
                        surface.tearing_failures += 1;
                        // Not on the first refusal. Turning tearing on changes
                        // which planes a frame uses, and the commit that
                        // carries that change is the one most likely to be
                        // refused — giving up there would mean a display that
                        // can tear never does.
                        if surface.tearing_failures >= 3 {
                            surface.refuses_tearing = true;
                            tracing::warn!(
                                "{}: the display refused a tearing flip three times, so it \
                                 will not be asked again",
                                output.name()
                            );
                        } else {
                            tracing::warn!(
                                "{}: the display refused a tearing flip ({} of 3)",
                                output.name(),
                                surface.tearing_failures
                            );
                        }
                        self.dirty_outputs.insert(id);
                        // And not a GPU error. The refusal has been answered
                        // here — tearing is off and the frame is queued again
                        // as a whole one — but `stalled()` reads any error at
                        // all as a device worth resuming, so leaving the count
                        // up means the next watchdog tick takes `Step::Resume`:
                        // reset_buffers and a full repaint on a display that is
                        // working and has already stopped being asked to tear.
                        failed = false;
                    }
                } else {
                    submitted = true;
                    surface.pending = true;
                    // Started here and stopped by the vblank. A flip that never
                    // comes back is the quiet way a GPU reset freezes a screen.
                    surface.queued_at = Some(std::time::Instant::now());
                    // The flip is away. Whatever commit started the wait has
                    // been carried, so time it and start the next wait from
                    // the next commit.
                    flipped_at = udev_commit_at.take();
                    if !surface.drawn {
                        surface.drawn = true;
                        tracing::info!("{}: first frame queued", output.name());
                    }
                }
            }
            // Nothing changed, so nothing is submitted — and with no frame
            // queued there is no vblank, so rendering stops until something
            // asks for it again. Correct for a static screen, and worth saying
            // out loud because it looks identical to being stuck.
            //
            // This is also the only pass worth holding back. A pass
            // that submits does not need holding: the flip it queued ends in a
            // vblank, and the vblank draws the next one. Only the empty pass
            // repeats without limit, and only the empty pass is timed.
            Ok(_) => {
                // The chain ends here: no flip means no vblank, so nothing
                // will drive the next frame until something asks again.
                let after_commit = udev.committed_since_flip;
                if let Some(log) = udev.frame_log.as_mut() {
                    log.empty += 1;
                    if after_commit {
                        log.empty_after_commit += 1;
                    }
                }
                tracing::debug!("{}: nothing to draw", output.name());
            }
            Err(e) => {
                // A renderer whose device has been lost fails here and goes on
                // failing: `VK_ERROR_DEVICE_LOST` is permanent for the
                // `VkDevice` that took it, and nothing but a new one helps. So
                // this is counted too, and the watchdog rebuilds the renderer
                // if the count keeps going up.
                tracing::warn!("render_frame: {e}");
                failed = true;
            }
        }

        // After the match, not inside it: the arm that queues the flip holds a
        // mutable borrow of the surface, and that borrow is of all of `udev`.
        if submitted {
            if let Some(log) = udev.frame_log.as_mut() {
                log.flips += 1;
            }
        }
        if failed {
            if let Some(device) = udev.devices.get_mut(id.device) {
                device.errors = device.errors.saturating_add(1);
            }
        }
        // Something is outstanding on this device: either a flip that has to
        // come back, or a failure that has to be answered. Either way there is
        // now a deadline, and the watchdog is what holds it. `self.udev` is out
        // on the caller's stack here, and none of this touches it.
        if submitted || failed {
            self.watch_gpus();
        }
        // The wait this flip ended, and the wait the next one inherits. Both
        // out here for the same reason the flip count is.
        udev.first_commit_at = udev_commit_at;
        if let Some(waited) = flipped_at.map(|at| at.elapsed()) {
            if let Some(log) = udev.frame_log.as_mut() {
                log.commit_to_flip_nanos += waited.as_nanos() as u64;
                log.commit_to_flip_count += 1;
                log.commit_to_flip_worst = log.commit_to_flip_worst.max(waited);
            }
        }

        if *pending_dump {
            self.dirty_outputs.insert(id);
        }

        // Screenshots, now that the frame this output shows has been drawn.
        // The renderer is moved out and put back because servicing needs the
        // whole compositor as well as the renderer, and the renderer lives
        // inside it — a copy composites the desktop, which is everything.
        // Anything sharing this screen, fed from the frame just drawn.
        if !self.casts.is_empty() {
            {
                // Before the frames: a source that has resized needs the
                // format agreed again, and the buffers for it come from this
                // renderer — which is why this is here and not inside
                // `feed_casts`. The state does not hold the renderer while it
                // is lent out, so anything reaching for `self.udev` in there
                // finds nothing.
                // Only the Vulkan renderer can allocate the DMA-BUFs a cast
                // hands over; under GLES a share takes the shared-memory path,
                // so there is nothing to resize here.
                self.resize_casts(None::<&mut VulkanRenderer>);
                self.feed_casts::<_, <R as Captures>::Buffer>(output, renderer);
            }
        }

        if !self.pending_copies.is_empty() || !self.pending_capture_frames.is_empty() {
            {
                self.service_screencopy::<_, <R as Captures>::Buffer>(output, renderer);
                self.service_image_capture::<_, <R as Captures>::Buffer>(output, renderer);
            }
        }

        // Frame callbacks, at most one per refresh per surface.
        //
        // A callback means "draw the next one", and sending one after a render
        // that found nothing to draw invites a frame that changes nothing,
        // whose commit marks the output dirty, whose render finds nothing to
        // draw, which sends another callback — a loop between compositor and
        // client that runs as fast as both can manage. With a browser and a
        // terminal open that was ten thousand renders a second and a core of
        // CPU, all of it logged as "nothing to draw".
        //
        // Withholding them entirely is worse, and was tried: a client that
        // paints only when invited never paints again, so the desktop takes
        // input and shows nothing — a freeze that looks like a hang and is
        // not. The throttle is the answer to both. Smithay skips a surface
        // that already had a callback within it, so a client that wants to
        // paint every frame still can, and one waiting on an invitation still
        // gets one.
        let throttle = Some(self.frame_interval());
        let _ = submitted;

        for window in self.space.elements() {
            window.send_frame(output, start, throttle, |_, _| Some(output.clone()));
        }
        for layer in smithay::desktop::layer_map_for_output(output).layers() {
            layer.send_frame(output, start, throttle, |_, _| Some(output.clone()));
        }
        // The lock screen too. It is not in the space and not a layer surface,
        // so nothing else reaches it — and a locker that never gets a frame
        // callback draws once and then stops, which is a lock screen whose
        // indicator never appears no matter what is typed.
        for lock in self.lock_surfaces.values() {
            smithay::desktop::utils::send_frames_surface_tree(
                lock.wl_surface(),
                output,
                start,
                throttle,
                |_, _| Some(output.clone()),
            );
        }
        // And the out-of-process shell, for exactly the same reason: it is not
        // in the space and not a layer surface. The frame clock also invites
        // it, but only while nothing is flipping — `frame_tick` stands down as
        // soon as a vblank has arrived recently — so on a running screen this
        // is the only invitation it ever gets. Without it the shell paints its
        // first frame and stops, and the desktop is a photograph.
        for surface in self.shell_client_surfaces() {
            smithay::desktop::utils::send_frames_surface_tree(
                &surface,
                output,
                start,
                throttle,
                |_, _| Some(output.clone()),
            );
        }
        // The wallpaper terminal, which is in neither the space nor a layer
        // map for the same reason the shell is not: it is one surface across
        // the whole layout. Without this it paints its first frame and stops,
        // which is a clock on the desktop that never ticks.
        if let Some(surface) = self.background_surface_for(output).cloned() {
            smithay::desktop::utils::send_frames_surface_tree(
                &surface,
                output,
                start,
                throttle,
                |_, _| Some(output.clone()),
            );
        }
        self.space.refresh();
        self.popups.cleanup();
        let _ = self.display_handle.flush_clients();
    }

    /// The VT was switched away from. Every device fd is about to be revoked.
    pub fn on_session_paused(&mut self) {
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        udev.active = false;
        // Every device, not just the first. The line above this function says
        // every fd is about to be revoked, and it is: logind takes them all
        // back on a VT switch. Pausing only the primary left every secondary
        // GPU's `DrmDevice` believing it still held a live fd.
        for device in udev.devices.iter_mut().filter(|device| device.online) {
            device.manager.pause();
        }
        tracing::info!("session paused");
    }

    /// The VT came back. Devices have to be reclaimed and every surface reset,
    /// because another compositor may have changed the mode while we were away.
    pub fn on_session_resumed(&mut self) {
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        udev.active = true;
        // Every device, for the reason the pause is: they were all revoked.
        //
        // Reactivating only the primary is a monitor on a second GPU that goes
        // dark at the first VT switch and never comes back — while everything
        // downstream carries on as though it had. The buffer reset below walks
        // every surface on every device, and the render loop at the end walks
        // every crtc, so the frames were being drawn and committed to devices
        // that had not been told they were live again.
        //
        // Each is reported on its own. One card failing to come back is one
        // dark monitor, which is worth a line naming it; taking the session
        // down for it would make every monitor dark.
        for (index, device) in udev.devices.iter_mut().enumerate() {
            // A slot whose card is gone has nothing to reclaim; the watchdog is
            // what brings that one back, and it stays out of the way until the
            // session is active again.
            if !device.online {
                continue;
            }
            if let Err(e) = device.manager.lock().activate(true) {
                tracing::error!("reactivating drm on gpu {index}: {e}");
            }
            // Whatever failed while the fds were revoked failed for that reason
            // and not because the card had crashed. Left standing, the count
            // would send the watchdog down the recovery ladder on the first tick
            // after every VT switch.
            device.errors = 0;
            device.recovery = crate::recovery::Step::Healthy;
            device.stepped_at = None;
        }
        tracing::info!("session resumed");

        // Another compositor had the screens while we were away, so nothing
        // in the damage history describes what is on them now.
        for surface in udev.surfaces_mut() {
            surface.drm_output.reset_buffers();
            surface.pending = false;
            surface.queued_at = None;
        }

        let crtcs: Vec<OutputId> = udev.ids();
        // The kernel reset every gamma ramp when the session was handed over,
        // and the client that set one has no way to know.
        self.restore_gamma();
        for id in crtcs {
            self.render(id);
        }
    }
}

/// Whether elements may be put on DRM planes instead of being composited.
///
/// On by default: it is what makes a fullscreen video cost nothing to display.
/// Whether a given element can stay on a plane is decided per frame, though —
/// a buffer whose format or modifier the plane will not take falls back to
/// composition — so an element that qualifies on one frame and not the next
/// alternates between two paths, which is visible as flicker.
///
/// VIEWPORT_SCANOUT=0 composites everything, which is slower and always
/// correct. It is a diagnostic: it tells "the planes are wrong" apart from
/// "the renderer is wrong" in one run, and those look identical on screen.
fn frame_flags() -> FrameFlags {
    match std::env::var("VIEWPORT_SCANOUT").as_deref() {
        Ok("0") => FrameFlags::empty(),
        _ => FrameFlags::DEFAULT,
    }
}

/// A CRTC this connector can drive that nothing else is using.
fn free_crtc(
    manager: &Manager,
    resources: &smithay::reexports::drm::control::ResourceHandles,
    connector: &connector::Info,
    taken: &std::collections::HashSet<crtc::Handle>,
) -> Option<crtc::Handle> {
    let device = manager.device();
    for encoder in connector.encoders() {
        let Ok(encoder) = device.get_encoder(*encoder) else {
            continue;
        };
        for crtc in resources.filter_crtcs(encoder.possible_crtcs()) {
            // A CRTC drives one connector at a time. Handing out one that is
            // already in use is how a second monitor ends up with no output.
            if !taken.contains(&crtc) {
                return Some(crtc);
            }
        }
    }
    None
}

/// Whether a connector is something the compositor should not drive.
///
/// The `non-desktop` property is how a head-mounted display says it is not a
/// monitor: driving it as one puts the desktop on a screen strapped to
/// somebody's face, at the wrong projection, and takes the connector away from
/// the client that could have used it properly. Absent property means an
/// ordinary display, which is every other connector on every other machine.
fn non_desktop(device: &DrmDevice, connector: connector::Handle) -> bool {
    use smithay::reexports::drm::control::Device as _;

    let Ok(properties) = device.get_properties(connector) else {
        return false;
    };
    for (handle, value) in properties {
        let Ok(info) = device.get_property(handle) else {
            continue;
        };
        if info.name().to_str() == Ok("non-desktop") {
            return value != 0;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<Option<String>> {
        list.iter().map(|p| Some((*p).to_owned())).collect()
    }

    #[test]
    fn nothing_asked_for_leaves_the_ranking_alone() {
        // The single-GPU path, and the default everywhere: this must not
        // change what the compositor already did.
        let cards = paths(&["/dev/dri/card0", "/dev/dri/card1"]);
        assert_eq!(gpu_named(&cards, None), Choice::Ranked);
        assert_eq!(gpu_named(&cards, Some("")), Choice::Ranked);
        assert_eq!(gpu_named(&cards, Some("   ")), Choice::Ranked);
    }

    #[test]
    fn the_seat_primary_is_not_listed_twice() {
        // all_gpus() includes the primary, and the candidate list puts the
        // primary in front of it, so every seat's primary appeared twice.
        // Harmless to the ranking and not to what follows it: the loop that
        // opens the other GPUs skips only the card that was chosen, so a
        // choice that was not the seat's primary opened that primary twice —
        // once successfully, then again to a failure whose warning says the
        // outputs are dark while they are lit.
        assert_eq!(first_occurrences([1, 2, 1, 3]), vec![1, 2, 3]);
        // The primary in front is the whole reason this is not a sort: the
        // ranking reads the order.
        assert_eq!(first_occurrences([2, 1, 2, 3]), vec![2, 1, 3]);
        assert_eq!(first_occurrences([7, 7, 7]), vec![7]);
        assert!(first_occurrences(Vec::<u8>::new()).is_empty());
    }

    #[test]
    fn a_hybrid_laptop_can_name_the_gpu_it_wants() {
        // The case this exists for. Both GPUs rank identically, so which one
        // was used came down to the order the seat listed them.
        let cards = paths(&["/dev/dri/card0", "/dev/dri/card1"]);
        assert_eq!(gpu_named(&cards, Some("card1")), Choice::Named(1));
        assert_eq!(gpu_named(&cards, Some("card0")), Choice::Named(0));
    }

    #[test]
    fn any_form_of_the_path_matches() {
        // Nobody remembers whether a given machine calls it card1 or
        // renderD129, and the by-path names are the only stable ones.
        let cards = paths(&[
            "/dev/dri/by-path/pci-0000:00:02.0-card",
            "/dev/dri/by-path/pci-0000:01:00.0-card",
        ]);
        assert_eq!(gpu_named(&cards, Some("0000:01:00.0")), Choice::Named(1));
        assert_eq!(
            gpu_named(&cards, Some("pci-0000:00:02.0")),
            Choice::Named(0)
        );
    }

    #[test]
    fn a_name_that_matches_nothing_is_reported_not_ignored() {
        // Silently ranking would leave a hybrid laptop on the wrong GPU with
        // the variable meant to fix it sitting in the environment doing
        // nothing — which is indistinguishable from the feature not existing.
        let cards = paths(&["/dev/dri/card0"]);
        assert_eq!(gpu_named(&cards, Some("card9")), Choice::NotFound);
        assert_eq!(gpu_named(&[], Some("card0")), Choice::NotFound);
    }

    #[test]
    fn a_card_with_no_path_is_skipped_rather_than_matched() {
        // dev_path() is an Option, and a None must not be treated as a match
        // for everything — that would pick an unnameable device.
        let cards = vec![None, Some("/dev/dri/card1".to_owned())];
        assert_eq!(gpu_named(&cards, Some("card1")), Choice::Named(1));
        assert_eq!(gpu_named(&cards, Some("card0")), Choice::NotFound);
    }
}
