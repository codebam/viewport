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

smithay::backend::renderer::element::render_elements! {
    /// Everything one output draws.
    ///
    /// Two kinds, because the shell is not a Wayland client: it is a texture
    /// imported straight from WebKit's DMA-BUF, sharing nothing with a
    /// surface but the renderer it is sampled by.
    pub OutputElement<=VulkanRenderer>;
    Surface=WaylandSurfaceRenderElement<VulkanRenderer>,
    /// A window cropped to the hole the shell drew for it. The shell measures
    /// that hole in its own DOM and it is not always the window's rectangle —
    /// during an open animation the window slides up from under the bar, and
    /// in a scrolling layout a column is half off the screen.
    CroppedSurface=smithay::backend::renderer::element::utils::CropRenderElement<
        WaylandSurfaceRenderElement<VulkanRenderer>,
    >,
    Shell=TextureRenderElement<viewport_vulkan::VulkanTexture>,
    Cursor=smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement<VulkanRenderer>,
}

type Exporter = GbmFramebufferExporter<DrmDeviceFd>;

type Manager = DrmOutputManager<GbmAllocator<DrmDeviceFd>, Exporter, (), DrmDeviceFd>;

/// One CRTC being driven.
pub struct Surface {
    pub output: Output,
    /// The connector this CRTC is driving, so a second pass can tell an
    /// output that is already up from one that still needs a CRTC.
    pub connector: connector::Handle,
    pub drm_output: DrmOutput<GbmAllocator<DrmDeviceFd>, Exporter, (), DrmDeviceFd>,
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
pub struct Udev {
    pub session: LibSeatSession,
    pub renderer: VulkanRenderer,
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
    let card = primary_gpu(&seat)
        .ok()
        .flatten()
        .or_else(|| all_gpus(&seat).ok()?.into_iter().next())
        .and_then(|path| DrmNode::from_path(path).ok())
        .and_then(|node| match node.ty() {
            NodeType::Primary => Some(node),
            _ => node.node_with_type(NodeType::Primary)?.ok(),
        })
        .ok_or_else(|| anyhow!("no GPU with a primary node found for seat {seat}"))?;

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

    state.udev = Some(Udev {
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
) -> Result<(Manager, VulkanRenderer, smithay::backend::drm::DrmDeviceNotifier)> {
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

    let vulkan = VulkanDevice::for_node(&instance, render)
        .context("opening a vulkan device on the primary GPU")?;

    // SCANOUT as well as RENDERING: these buffers go to the display
    // controller, and a buffer allocated without it may not be scannable.
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );

    let renderer = VulkanRenderer::with_allocator(&vulkan, allocator.clone())
        .map_err(|e| anyhow!("creating the vulkan renderer: {e}"))?;

    let render_formats = smithay::backend::renderer::ImportDma::dmabuf_formats(&renderer);

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

            // Already up, from an earlier pass.
            if udev
                .surfaces
                .values()
                .any(|s| s.connector == connector.handle())
            {
                continue;
            }

            // The mode the display says it prefers, or the first it lists.
            let Some(mode) = connector
                .modes()
                .iter()
                .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
                .or_else(|| connector.modes().first())
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
            let result = udev.manager.lock().initialize_output::<
                _,
                WaylandSurfaceRenderElement<VulkanRenderer>,
            >(
                crtc,
                mode,
                &[connector.handle()],
                &output,
                None,
                &mut udev.renderer,
                &Default::default(),
            );

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

    /// The pointer, as elements for one output.
    ///
    /// Empty when the pointer is elsewhere, or when the client asked for it to
    /// be hidden — `CursorImageStatus::Hidden` is a request to draw nothing,
    /// not to fall back to the theme.
    fn cursor_element(
        &mut self,
        output_geometry: Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>>,
        scale: f64,
    ) -> Vec<OutputElement> {
        use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
        use smithay::input::pointer::CursorImageStatus;

        let Some(output_geometry) = output_geometry else {
            return Vec::new();
        };
        let Some(pointer) = self.seat.get_pointer() else {
            return Vec::new();
        };
        let location = pointer.current_location();
        if !output_geometry.to_f64().contains(location) {
            return Vec::new();
        }
        // Relative to this output, which is the space every element is in.
        let local = (location - output_geometry.loc.to_f64()).to_physical(scale);

        match self.cursor_status.clone() {
            CursorImageStatus::Hidden => Vec::new(),

            // The client's own surface. Its hotspot is stored on the surface
            // by the seat, and the surface is drawn with that subtracted so
            // the point the user aims with is where the pointer is.
            CursorImageStatus::Surface(surface) => {
                use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
                use smithay::wayland::compositor::with_states;

                let hotspot = with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<std::sync::Mutex<smithay::input::pointer::CursorImageAttributes>>()
                        .map(|attrs| attrs.lock().unwrap().hotspot)
                        .unwrap_or_default()
                });
                let at = (local.to_f64() - hotspot.to_f64().to_physical(scale)).to_i32_round();
                let Some(udev) = self.udev.as_mut() else {
                    return Vec::new();
                };
                render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<VulkanRenderer>>(
                    &mut udev.renderer,
                    &surface,
                    at,
                    scale,
                    1.0,
                    smithay::backend::renderer::element::Kind::Cursor,
                )
                .into_iter()
                .map(OutputElement::from)
                .collect()
            }

            CursorImageStatus::Named(shape) => {
                let millis = self.start_time.elapsed().as_millis() as u32;
                let Some((buffer, hotspot)) =
                    self.cursor_theme
                        .image(shape.name(), scale.ceil() as i32, millis)
                else {
                    // No theme installed, or none with this shape. Drawing
                    // nothing is better than a wrong image, and saying so once
                    // beats a pointer that is silently absent.
                    if !self.cursor_warned {
                        self.cursor_warned = true;
                        tracing::warn!(
                            "no xcursor image for {:?}; set XCURSOR_THEME to a theme that is installed",
                            shape.name()
                        );
                    }
                    return Vec::new();
                };
                let Some(udev) = self.udev.as_mut() else {
                    return Vec::new();
                };
                MemoryRenderBufferRenderElement::from_buffer(
                    &mut udev.renderer,
                    (local.to_f64() - hotspot.to_f64()),
                    &buffer,
                    None,
                    None,
                    None,
                    smithay::backend::renderer::element::Kind::Cursor,
                )
                .ok()
                .map(OutputElement::from)
                .into_iter()
                .collect()
            }
        }
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
        if let Err(e) = surface.drm_output.frame_submitted() {
            tracing::warn!("frame_submitted: {e}");
        }
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

        let Some(udev) = self.udev.as_mut() else {
            return;
        };
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
            self.needs_render = true;
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
        udev.renderer.set_output_description(description);

        let elements = crate::render::build(&frame, &mut udev.renderer);

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
                pending_dump = true;
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
                if let Err(e) = crate::dump::output_frame(
                    &mut udev.renderer,
                    &elements,
                    size,
                    [0.1, 0.1, 0.1, 1.0],
                    &path,
                ) {
                    tracing::error!("could not dump {}: {e:#}", output.name());
                }
            }
        }

        let result = surface.drm_output.render_frame(
            &mut udev.renderer,
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
                if let Err(e) = surface.drm_output.queue_frame(()) {
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
                        self.needs_render = true;
                    }
                } else {
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
            Ok(_) => tracing::debug!("{}: nothing to draw", output.name()),
            Err(e) => tracing::warn!("render_frame: {e}"),
        }

        if pending_dump {
            self.needs_render = true;
        }

        // Screenshots, now that the frame this output shows has been drawn.
        // The renderer is moved out and put back because servicing needs the
        // whole compositor as well as the renderer, and the renderer lives
        // inside it — a copy composites the desktop, which is everything.
        // Anything sharing this screen, fed from the frame just drawn.
        if !self.casts.is_empty() {
            if let Some(mut udev) = self.udev.take() {
                // Before the frames: a source that has resized needs the
                // format agreed again, and the buffers for it come from this
                // renderer — which is why this is here and not inside
                // `feed_casts`. The state does not hold the renderer while it
                // is lent out, so anything reaching for `self.udev` in there
                // finds nothing.
                self.resize_casts(Some(&mut udev.renderer));
                self.feed_casts::<_, smithay::backend::allocator::dmabuf::Dmabuf>(
                    &output,
                    &mut udev.renderer,
                );
                self.udev = Some(udev);
            }
        }

        if !self.pending_copies.is_empty() || !self.pending_capture_frames.is_empty() {
            if let Some(mut udev) = self.udev.take() {
                self.service_screencopy::<_, smithay::backend::allocator::dmabuf::Dmabuf>(
                    &output,
                    &mut udev.renderer,
                );
                self.service_image_capture::<_, smithay::backend::allocator::dmabuf::Dmabuf>(
                    &output,
                    &mut udev.renderer,
                );
                self.udev = Some(udev);
            }
        }

        // Anything blocked waiting for this frame: a client pacing itself with
        // wp-fifo, or timing a commit with wp-commit-timing. Released before
        // the frame callbacks, because both are about *this* frame having
        // happened.
        self.release_frame_barriers(&output, start);
        // And if anything is still waiting — a barrier set by the commit this
        // frame just drew — keep a clock on it. Without this the next blocked
        // commit produces no damage and nothing draws again.
        self.arm_barrier_tick();

        // Frame callbacks: a client will not paint again until it gets one.
        for window in self.space.elements() {
            window.send_frame(&output, start, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
        for layer in smithay::desktop::layer_map_for_output(&output).layers() {
            layer.send_frame(&output, start, Some(Duration::ZERO), |_, _| {
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
                Some(Duration::ZERO),
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
