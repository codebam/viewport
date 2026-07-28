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
}

/// Everything the DRM backend holds.
pub struct Udev {
    pub session: LibSeatSession,
    pub renderer: VulkanRenderer,
    pub manager: Manager,
    pub surfaces: HashMap<crtc::Handle, Surface>,
    pub node: DrmNode,
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
        active: true,
    });
    state.on_connectors_changed();

    #[cfg(feature = "wpe")]
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
                        },
                    );
                    started.push(crtc);
                }
                Err(e) => tracing::warn!("{name}: could not initialise: {e}"),
            }
        }

        self.notify_output_layout();

        // Draw once per new output. Rendering is driven by vblank, and vblank
        // only arrives after a frame has been queued — so without this first
        // push nothing ever draws and the compositor sits on a modeset screen
        // doing nothing, which is exactly what the first run on real hardware
        // did.
        for crtc in started {
            self.render(crtc);
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
        if let Err(e) = surface.drm_output.frame_submitted() {
            tracing::warn!("frame_submitted: {e}");
        }
        self.render(crtc);
    }

    /// Draw one output.
    pub fn render(&mut self, crtc: crtc::Handle) {
        let start = self.start_time.elapsed();
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        if !udev.active {
            return;
        }
        let Some(surface) = udev.surfaces.get_mut(&crtc) else {
            return;
        };
        let output = surface.output.clone();

        let elements: Vec<WaylandSurfaceRenderElement<VulkanRenderer>> = Vec::new();

        let result = surface.drm_output.render_frame(
            &mut udev.renderer,
            &elements,
            // Behind everything, and behind the shell too — visible only
            // where nothing else covers it.
            [0.1, 0.1, 0.1, 1.0],
            FrameFlags::DEFAULT,
        );

        match result {
            Ok(frame) if !frame.is_empty => {
                if let Err(e) = surface.drm_output.queue_frame(()) {
                    tracing::warn!("queue_frame: {e}");
                } else if !surface.drawn {
                    surface.drawn = true;
                    tracing::info!("{}: first frame queued", output.name());
                }
            }
            // Nothing changed, so nothing is submitted — and with no frame
            // queued there is no vblank, so rendering stops until something
            // asks for it again. Correct for a static screen, and worth
            // saying out loud because it looks identical to being stuck.
            Ok(_) => tracing::debug!("{}: nothing to draw", output.name()),
            Err(e) => tracing::warn!("render_frame: {e}"),
        }

        // Frame callbacks: a client will not paint again until it gets one.
        for window in self.space.elements() {
            window.send_frame(&output, start, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
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

        let crtcs: Vec<crtc::Handle> = udev.surfaces.keys().copied().collect();
        for crtc in crtcs {
            self.render(crtc);
        }
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
