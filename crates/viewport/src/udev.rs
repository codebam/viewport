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
use std::path::Path;
use std::time::Duration;

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
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::AsRenderElements as _;
#[cfg(feature = "wpe")]
use smithay::backend::renderer::element::Kind;
#[cfg(feature = "wpe")]
use smithay::backend::renderer::Renderer as _;
use smithay::desktop::Window;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::drm::control::{connector, crtc, Device as _, ModeTypeFlags};
use smithay::reexports::rustix::fs::OFlags;
use smithay::utils::{DeviceFd, Physical, Point, Rectangle};

use viewport_vulkan::{Device as VulkanDevice, VulkanRenderer};

use crate::state::ViewportState;

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
}

/// Everything the DRM backend holds.
/// The renderer the DRM path draws with.
///
/// Vulkan wherever it can: it is the one with colour management and explicit
/// sync. GLES exists for the machines Vulkan cannot serve — a virtual machine,
/// where software Vulkan lacks `VK_EXT_image_drm_format_modifier` and Venus
/// aborts inside its own driver, while virgl gives OpenGL ES on the GBM
/// platform and works. Without this a guest shows nothing at all.
pub enum Gpu {
    Vulkan(VulkanRenderer),
    Gles(smithay::backend::renderer::gles::GlesRenderer),
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
            $crate::udev::Gpu::Gles($renderer) => $body,
            $crate::udev::Gpu::Placeholder => {
                panic!("the renderer was used while it was moved out")
            }
        }
    };
}

impl Gpu {
    /// The formats a client may hand over, whichever renderer this is.
    pub fn dmabuf_formats(&self) -> smithay::backend::allocator::format::FormatSet {
        use smithay::backend::renderer::ImportDma as _;
        match self {
            Gpu::Vulkan(renderer) => renderer.dmabuf_formats(),
            Gpu::Gles(renderer) => renderer.dmabuf_formats(),
            Gpu::Placeholder => Default::default(),
        }
    }

    pub fn is_vulkan(&self) -> bool {
        matches!(self, Gpu::Vulkan(_))
    }
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
    pub renderer: Gpu,
    pub manager: Manager,
    pub surfaces: HashMap<crtc::Handle, Surface>,
    pub node: DrmNode,
    /// True while the outputs are off because the session went idle.
    pub blanked: bool,
    /// False between a VT switch away and the switch back. Every device fd is
    /// revoked in that window, so committing a frame would fail.
    pub active: bool,
}

/// Bring up the backend.
pub fn init(event_loop: &mut EventLoop<'static, ViewportState>, state: &mut ViewportState) -> Result<()> {
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
    let candidates: Vec<DrmNode> = primary_gpu(&seat)
        .ok()
        .flatten()
        .into_iter()
        .chain(all_gpus(&seat).unwrap_or_default())
        .filter_map(|path| DrmNode::from_path(path).ok())
        .filter_map(|node| match node.ty() {
            NodeType::Primary => Some(node),
            _ => node.node_with_type(NodeType::Primary)?.ok(),
        })
        .collect();
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

    let card = candidates
        .iter()
        .find(|card| renders_for(card))
        .copied()
        .unwrap_or_else(|| {
            // Nothing matched. The fallback in `for_node` will draw on
            // whatever Vulkan device there is, which is right for a machine
            // with one GPU and a software renderer and wrong for nothing.
            tracing::warn!(
                "no GPU has a Vulkan device of its own; going with what the seat calls primary"
            );
            candidates[0]
        });
    if candidates.len() > 1 {
        tracing::info!(
            "{} GPUs; drawing and scanning out on {card:?}",
            candidates.len()
        );
    }

    // Same card, the node Vulkan should use.
    let render = card
        .node_with_type(NodeType::Render)
        .and_then(|node| node.ok())
        .unwrap_or(card);

    tracing::info!("primary GPU: card {card:?}, render {render:?}");
    // Said out loud because it changes what the screen does, and a run whose
    // settings are not in its log cannot be compared with another one.
    tracing::info!(
        "scanout {}",
        if frame_flags().is_empty() { "off" } else { "on" }
    );

    let mut session = session;
    let (manager, renderer, drm_notifier) = open_device(&mut session, &card, &render)?;

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
    event_loop
        .handle()
        .insert_source(drm_notifier, move |event, metadata, state| match event {
            DrmEvent::VBlank(crtc) => state.on_vblank(crtc, metadata),
            DrmEvent::Error(error) => tracing::error!("drm: {error}"),
        })
        .map_err(|e| anyhow!("inserting the drm source: {e}"))?;

    // Session: the VT was switched away from or back to.
    event_loop
        .handle()
        .insert_source(notifier, move |event, _, state| match event {
            SessionEvent::PauseSession => state.on_session_paused(),
            SessionEvent::ActivateSession => state.on_session_resumed(),
        })
        .map_err(|e| anyhow!("inserting the session source: {e}"))?;

    // Connector hotplug. Whole-device hotplug is not handled; a GPU appearing
    // mid-session is not something this compositor claims to survive.
    let udev = UdevBackend::new(&seat).map_err(|e| anyhow!("udev: {e}"))?;
    event_loop
        .handle()
        .insert_source(udev, move |event, _, state| match event {
            UdevEvent::Changed { device_id } => {
                if DrmNode::from_dev_id(device_id).is_ok() {
                    state.on_connectors_changed();
                }
            }
            UdevEvent::Added { .. } | UdevEvent::Removed { .. } => {}
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
            .insert_source(source, |_, _, state| state.drain_shell())
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
        renderer,
        manager,
        surfaces: HashMap::new(),
        node: card,
        blanked: false,
        active: true,
    });

    // Claim DRM master before anything is committed.
    //
    // The session is already active when the compositor starts on a TTY, so
    // no ActivateSession event ever arrives and nothing else would take it.
    // Without this the initial modeset appears to work and every page flip
    // afterwards fails with EPERM — which presents as a screen that stops
    // updating rather than as anything anyone would connect to permissions.
    if let Some(udev) = state.udev.as_mut() {
        if let Err(e) = udev.manager.lock().activate(false) {
            tracing::error!("could not claim drm master: {e}");
        } else {
            tracing::info!("drm master claimed");
        }
    }

    // Now that there is a renderer, clients can be told which formats they may
    // allocate — and on which GPU.
    {
        use smithay::backend::renderer::ImportDma as _;
        let formats = state
            .udev
            .as_ref()
            .map(|udev| udev.renderer.dmabuf_formats().iter().copied().collect())
            .unwrap_or_default();
        state.advertise_dmabuf(Some(render.dev_id()), formats);
    }

    state.on_connectors_changed();

    // Now that the outputs exist, the config file can say what they should be.
    state.apply_output_config();
    if state.adaptive_sync {
        state.set_adaptive_sync(true);
    }

    // What a capture client may allocate on: the render node, not the card.
    // Allocation happens there, and it is the node a client may open without
    // being the session's master.
    {
        use smithay::backend::renderer::ImportDma as _;
        let formats: Vec<_> = state
            .udev
            .as_ref()
            .map(|udev| udev.renderer.dmabuf_formats().iter().copied().collect())
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
            .map(|udev| udev.manager.device().device_fd().clone());
        if let Some(import_device) = import_device {
            if smithay::wayland::drm_syncobj::supports_syncobj_eventfd(&import_device) {
                state.syncobj_state =
                    Some(smithay::wayland::drm_syncobj::DrmSyncobjState::new::<ViewportState>(
                        &state.display_handle,
                        import_device,
                    ));
                tracing::info!("explicit sync is available on this gpu");
            } else {
                tracing::info!("this gpu has no syncobj eventfd; implicit sync only");
            }
        }
    }

    #[cfg(feature = "wpe")]
    // Sizes, maps and focuses the view itself — WebKit paints nothing into an
    // unmapped view of no size.
    state.start_shell(&card, &render)?;

    Ok(())
}

/// Open the DRM device and build everything that hangs off it.
fn open_device(
    session: &mut LibSeatSession,
    card: &DrmNode,
    render: &DrmNode,
) -> Result<(Manager, Gpu, smithay::backend::drm::DrmDeviceNotifier)> {
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
        "gles" | "gl" | "opengl" => {
            tracing::info!("VIEWPORT_RENDERER={asked}: the OpenGL renderer");
            Gpu::Gles(gles_renderer(&gbm)?)
        }
        _ => {
            let vulkan = VulkanDevice::for_node(&instance, render)
                .context("opening a vulkan device on the primary GPU")
                .and_then(|device| {
                    VulkanRenderer::with_allocator(&device, allocator.clone())
                        .map_err(|e| anyhow!("creating the vulkan renderer: {e}"))
                });
            match vulkan {
                Ok(renderer) => Gpu::Vulkan(renderer),
                Err(e) if asked == "vulkan" => return Err(e),
                Err(e) => {
                    // A virtual machine is the usual reason: software Vulkan
                    // has no VK_EXT_image_drm_format_modifier and Venus aborts
                    // in its own driver, while virgl gives OpenGL ES on the
                    // GBM platform and works. Refusing here is a session that
                    // shows nothing, which is worse than one without colour
                    // management.
                    tracing::warn!("no usable Vulkan renderer ({e:#}); falling back to OpenGL");
                    Gpu::Gles(gles_renderer(&gbm)?)
                }
            }
        }
    };

    let render_formats = renderer.dmabuf_formats();

    // The exporter turns a rendered buffer into a DRM framebuffer handle. It
    // takes the node so it can tell a buffer allocated here from one imported
    // from another GPU.
    let exporter = GbmFramebufferExporter::new(gbm.clone(), (*render).into());

    let manager = DrmOutputManager::new(
        drm,
        allocator,
        exporter,
        Some(gbm),
        SCANOUT_FORMATS.iter().copied(),
        render_formats,
    );

    Ok((manager, renderer, notifier))
}

/// An OpenGL ES renderer on the same GBM device.
///
/// EGL rather than Vulkan, which is what makes it work where Vulkan cannot:
/// virgl in a guest exposes GL through the GBM platform, and this is the path
/// the nested backend has always used.
fn gles_renderer(gbm: &GbmDevice<DrmDeviceFd>) -> Result<smithay::backend::renderer::gles::GlesRenderer> {
    let display = unsafe { smithay::backend::egl::EGLDisplay::new(gbm.clone()) }
        .context("opening an EGL display on the GBM device")?;
    let context = smithay::backend::egl::EGLContext::new(&display)
        .context("creating an EGL context")?;
    // SAFETY: the context is current on this thread for the renderer's life,
    // which is the compositor's — the DRM path is single-threaded.
    Ok(unsafe { smithay::backend::renderer::gles::GlesRenderer::new(context) }
        .context("creating the OpenGL renderer")?)
}

impl ViewportState {
    /// Walk the connectors and bring up anything newly connected.
    pub fn on_connectors_changed(&mut self) {
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        if !udev.active {
            // The fds are revoked while switched away; asking the device
            // anything here fails and the answer would be stale by the time we
            // came back anyway.
            return;
        }

        // Outputs brought up by this pass, so the first frame can be kicked
        // once the borrow on `udev` is released.
        let mut started: Vec<crtc::Handle> = Vec::new();

        let device = udev.manager.device();
        let Ok(resources) = device.resource_handles() else {
            tracing::error!("could not read drm resources");
            return;
        };

        let connectors: Vec<connector::Info> = resources
            .connectors()
            .iter()
            .filter_map(|handle| device.get_connector(*handle, true).ok())
            .filter(|info| info.state() == connector::State::Connected)
            .collect();

        // Worked out here, while the device is borrowed for reading, because
        // offering one for lease needs the lease state — which is a mutable
        // borrow of the same `udev`.
        let leasable: std::collections::HashSet<connector::Handle> = connectors
            .iter()
            .filter(|info| non_desktop(device, info.handle()))
            .map(|info| info.handle())
            .collect();

        tracing::info!("{} connected connector(s)", connectors.len());

        // CRTCs already driving something. Without this the second monitor
        // gets handed the first one's CRTC, and the "already in use" check
        // then drops it silently — which is exactly what happened on the first
        // two-monitor run.
        let mut taken: std::collections::HashSet<crtc::Handle> =
            udev.surfaces.keys().copied().collect();

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
            if udev
                .surfaces
                .values()
                .any(|s| s.connector == connector.handle())
            {
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
            let chosen = wanted.and_then(|config| {
                crate::config::pick_mode(connector.modes(), config)
            });
            if let Some(mode) = chosen.as_ref() {
                tracing::info!(
                    "{name}: {}x{}@{} from the configuration",
                    mode.size().0,
                    mode.size().1,
                    mode.vrefresh()
                );
            }
            let Some(mode) = chosen.as_ref().or_else(|| connector
                .modes()
                .iter()
                .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
                .or_else(|| connector.modes().first()))
                .copied()
            else {
                continue;
            };

            let Some(crtc) = free_crtc(&udev.manager, &resources, &connector, &taken) else {
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
            let mut renderer = std::mem::replace(&mut udev.renderer, Gpu::Placeholder);
            let result = crate::with_gpu!(&mut renderer, |r| {
                udev.manager
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
            udev.renderer = renderer;

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

                    tracing::info!("{name}: {}x{} at x={x}", mode.size().0, mode.size().1);
                    self.space.map_output(&output, (x, 0));
                    if self.active_output.is_none() {
                        self.active_output = Some(name);
                    }
                    udev.surfaces.insert(
                        crtc,
                        Surface {
                            output,
                            connector: connector.handle(),
                            drm_output,
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
                        },
                    );
                    started.push(crtc);
                }
                Err(e) => tracing::warn!("{name}: could not initialise: {e}"),
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
    }

    /// Layer surfaces for one output, split above and below the windows.
    ///
    /// Overlay and top go in front — a lock screen, a launcher, a bar — and
    /// background and bottom go behind. Both are returned in output-local
    /// physical coordinates, which is the space every element is drawn in.
    fn layers_for(
        &self,
        crtc: &crtc::Handle,
        scale: f64,
    ) -> (
        Vec<(smithay::desktop::LayerSurface, Point<i32, Physical>)>,
        Vec<(smithay::desktop::LayerSurface, Point<i32, Physical>)>,
    ) {
        use smithay::wayland::shell::wlr_layer::Layer;

        let Some(output) = self
            .udev
            .as_ref()
            .and_then(|udev| udev.surfaces.get(crtc))
            .map(|surface| surface.output.clone())
        else {
            return (Vec::new(), Vec::new());
        };

        let map = smithay::desktop::layer_map_for_output(&output);
        let mut above = Vec::new();
        let mut below = Vec::new();
        for layer in map.layers() {
            let Some(geometry) = map.layer_geometry(layer) else {
                continue;
            };
            let location = geometry.loc.to_f64().to_physical(scale).to_i32_round();
            let entry = (layer.clone(), location);
            match layer.layer() {
                Layer::Overlay | Layer::Top => above.push(entry),
                Layer::Background | Layer::Bottom => below.push(entry),
            }
        }
        (above, below)
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

        let mut feedback =
            smithay::desktop::utils::OutputPresentationFeedback::new(output);
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
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(surface, None, states)
                },
            );
        }
        feedback
    }

    /// A frame finished scanning out, so the next one may be drawn.
    pub fn on_vblank(
        &mut self,
        crtc: crtc::Handle,
        metadata: &mut Option<smithay::backend::drm::DrmEventMetadata>,
    ) {
        let _ = metadata;
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        let Some(surface) = udev.surfaces.get_mut(&crtc) else {
            return;
        };
        // The flip happened, so this output may be drawn into again.
        surface.pending = false;
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
                let now = smithay::reexports::rustix::time::clock_gettime(
                    smithay::reexports::rustix::time::ClockId::Monotonic,
                );
                let clock = std::time::Duration::new(now.tv_sec as u64, now.tv_nsec as u32);
                let (sequence, flags) = match metadata.as_ref() {
                    Some(metadata) => (
                        metadata.sequence,
                        match metadata.time {
                            smithay::backend::drm::DrmEventTime::Monotonic(_) => {
                                smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync
                                    | smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::HwClock
                                    | smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::HwCompletion
                            }
                            _ => smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync,
                        },
                    ),
                    None => (
                        0,
                        smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync,
                    ),
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
            .and_then(|udev| udev.surfaces.get(&crtc))
            .map(|surface| surface.output.clone());
        if let Some(output) = output {
            let at = self.start_time.elapsed();
            self.release_frame_barriers(&output, at);
        }
        // And if anything is still waiting, keep a clock on it: a blocked
        // commit makes no damage, so without this nothing would draw again.
        self.arm_barrier_tick();

        self.render(crtc);
    }

    /// Draw one output.
    pub fn render(&mut self, crtc: crtc::Handle) {
        let start = self.start_time.elapsed();

        let Some(output) = self
            .udev
            .as_ref()
            .and_then(|udev| udev.surfaces.get(&crtc))
            .map(|surface| surface.output.clone())
        else {
            return;
        };

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
        let mut gpu = std::mem::replace(&mut udev.renderer, Gpu::Placeholder);
        // Colour management belongs to the Vulkan renderer; GLES draws in the
        // output's own space, so an HDR screen driven by it gets the ordinary
        // one — the honest result of that renderer not having the transforms.
        if let Gpu::Vulkan(renderer) = &mut gpu {
            let description = if udev
                .surfaces
                .get(&crtc)
                .map(|surface| surface.hdr)
                .unwrap_or(false)
            {
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
            crtc,
            &output,
            frame,
            wants_tearing,
            settled_for,
            start,
            &mut pending_dump,
        ));
        udev.renderer = gpu;
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
        crtc: crtc::Handle,
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
        let Some(surface) = udev.surfaces.get_mut(&crtc) else {
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
                if honoured { "" } else { " (this display cannot, so it will not)" }
            );
        }

        // What this output expects, before anything is drawn for it. One
        // renderer draws every monitor, so a description left over from an HDR
        // display converts the next one's frame as though it were HDR too.
        let description = if surface.hdr {
            viewport_vulkan::color::Description {
                primaries: viewport_vulkan::color::Primaries::BT2020,
                transfer: viewport_vulkan::color::TransferFunction::Pq,
                reference_luminance: 203.0,
            }
        } else {
            viewport_vulkan::color::Description::default()
        };
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
                let Some(surface) = udev.surfaces.get_mut(&crtc) else {
                    return;
                };
                if let Err(e) = surface.drm_output.queue_frame(Some(feedback)) {
                    // The vblank that would have driven the next frame never
                    // arrives, so a failure here stops the output for good
                    // rather than dropping one frame.
                    tracing::warn!("queue_frame: {e}");

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
                        self.dirty_outputs.insert(crtc);
                    }
                } else {
                    submitted = true;
                    surface.pending = true;
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
                tracing::debug!("{}: nothing to draw", output.name());
            }
            Err(e) => tracing::warn!("render_frame: {e}"),
        }

        if *pending_dump {
            self.dirty_outputs.insert(crtc);
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
                self.feed_casts::<_, <R as Captures>::Buffer>(
                    &output,
                    renderer,
                );
            }
        }

        if !self.pending_copies.is_empty() || !self.pending_capture_frames.is_empty() {
            {
                self.service_screencopy::<_, <R as Captures>::Buffer>(
                    &output,
                    renderer,
                );
                self.service_image_capture::<_, <R as Captures>::Buffer>(
                    &output,
                    renderer,
                );
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
            window.send_frame(&output, start, throttle, |_, _| {
                Some(output.clone())
            });
        }
        for layer in smithay::desktop::layer_map_for_output(&output).layers() {
            layer.send_frame(&output, start, throttle, |_, _| {
                Some(output.clone())
            });
        }
        // The lock screen too. It is not in the space and not a layer surface,
        // so nothing else reaches it — and a locker that never gets a frame
        // callback draws once and then stops, which is a lock screen whose
        // indicator never appears no matter what is typed.
        for lock in self.lock_surfaces.values() {
            smithay::desktop::utils::send_frames_surface_tree(
                lock.wl_surface(),
                &output,
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
        udev.manager.pause();
        tracing::info!("session paused");
    }

    /// The VT came back. Devices have to be reclaimed and every surface reset,
    /// because another compositor may have changed the mode while we were away.
    pub fn on_session_resumed(&mut self) {
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        udev.active = true;
        if let Err(e) = udev.manager.lock().activate(true) {
            tracing::error!("reactivating drm: {e}");
        }
        tracing::info!("session resumed");

        // Another compositor had the screens while we were away, so nothing
        // in the damage history describes what is on them now.
        for surface in udev.surfaces.values_mut() {
            surface.drm_output.reset_buffers();
            surface.pending = false;
        }

        let crtcs: Vec<crtc::Handle> = udev.surfaces.keys().copied().collect();
        // The kernel reset every gamma ramp when the session was handed over,
        // and the client that set one has no way to know.
        self.restore_gamma();
        for crtc in crtcs {
            self.render(crtc);
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

/// Whether a DRM device is present at all.
///
/// Used to decide whether the udev backend is worth trying before the session
/// is opened, so a machine with no GPU gets a clear message rather than a
/// libseat error.
pub fn available() -> bool {
    Path::new("/dev/dri").exists()
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
