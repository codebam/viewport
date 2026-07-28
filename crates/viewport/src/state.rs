// SPDX-License-Identifier: GPL-3.0-or-later
//
// Compositor state. Ports src/server.c.

use std::ffi::OsString;
use std::sync::Arc;

use smithay::desktop::{PopupManager, Space, Window, WindowSurfaceType};
use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{
    EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction,
};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;

use viewport_ipc::event::{Config, OutputInfo};
use viewport_ipc::{Event, Transform};

use smithay::xwayland::X11Wm;

use crate::ipc::Ipc;
use crate::views::{Views, NO_VIEW};

pub struct ViewportState {
    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, Self>,
    pub loop_signal: LoopSignal,

    pub space: Space<Window>,
    pub popups: PopupManager,

    /// The window registry. Separate from `space` on purpose — see
    /// [`crate::views`].
    pub views: Views,
    pub focused: u32,

    pub ipc: Ipc,
    pub shell_announced: bool,

    /// What the shell is told on connect, and what the config file patches.
    /// The built-in values are C's (`src/main.c:61`).
    pub config: Config,
    /// Where the shell is loaded from, when the config names somewhere.
    pub shell_url: Option<String>,
    /// The config file's `outputs` block, kept because an output named there
    /// may be plugged in later.
    pub output_config: std::collections::HashMap<String, crate::config::OutputConfig>,
    /// What to run once the compositor is up.
    pub startup: Option<String>,
    /// System statistics for the bar, sampled here because the page cannot.
    pub status: crate::status::Status,
    /// Locking and blanking after a while, off unless the file asks.
    pub idle: crate::idle::Idle,
    pub idle_settings: crate::idle::Settings,
    /// Whether Ctrl+Alt+F1..F12 may switch VT. A kiosk turns this off.
    pub vt_switching: bool,
    /// Whether clients are asked to let the compositor own the window frame.
    /// The shell draws one in DOM, so a client titlebar is a duplicate.
    pub server_decorations: bool,
    /// What the shell is told the system appearance is.
    pub dark_mode: bool,
    /// Variable refresh, where the display does it.
    pub adaptive_sync: bool,
    /// Where to go if the shell will not load, and how long to wait for its
    /// first painted frame.
    pub fallback_url: Option<String>,
    pub load_timeout_ms: u64,
    /// Whether the logo key was down last time it was looked at, so the shell
    /// hears about a change rather than about every keystroke.
    pub logo_held: bool,

    /// While the overview is up the shell draws miniatures of every window and
    /// a click means "go there" rather than reaching the client underneath.
    pub overview: bool,
    pub active_output: Option<String>,

    /// The DRM backend, when running on real hardware rather than nested.
    pub udev: Option<crate::udev::Udev>,

    /// Keys whose press was intercepted, so the matching release can be too.
    pub suppressed_keys: Vec<smithay::input::keyboard::Keysym>,

    /// Keybindings. Almost all of them are passthroughs to the shell.
    pub bindings: Vec<crate::binding::Binding>,

    /// Stops the outer GLib loop. calloop's own signal only ends the inner
    /// dispatch, so quitting has to go through this when the web engine is
    /// running.
    #[cfg(feature = "wpe")]
    pub glib: Option<crate::glib_loop::GlibSignal>,

    /// The web engine drawing the desktop, once it has started.
    #[cfg(feature = "wpe")]
    pub shell: Option<crate::shell::Shell>,

    /// Wakes the loop when the shell posts something.
    #[cfg(feature = "wpe")]
    pub shell_ping: Option<smithay::reexports::calloop::ping::Ping>,

    /// A renderer of the compositor's own, for copying WebKit's frames into
    /// buffers it owns. Independent of the backend — see `start_shell`.
    #[cfg(feature = "wpe")]
    pub shell_renderer: Option<viewport_vulkan::VulkanRenderer>,
    /// The size the shell was last told it is, so a layout change that does
    /// not alter it costs nothing.
    #[cfg(feature = "wpe")]
    pub shell_size: Option<(u32, u32)>,
    /// The compositor's own copy of the shell's newest frame, and its size.
    ///
    /// Reused between frames; reallocated only when the layout changes size.
    #[cfg(feature = "wpe")]
    pub shell_owned: Option<(
        smithay::backend::allocator::dmabuf::Dmabuf,
        smithay::utils::Size<i32, smithay::utils::Physical>,
    )>,
    /// How many frames the shell has painted. Only for the log: "one frame
    /// and then nothing" and "painting normally" are the same still picture.
    #[cfg(feature = "wpe")]
    pub shell_frames: u64,
    /// The shell element's identity, stable for the life of the compositor.
    ///
    /// A fresh `Id` per frame would make every damage tracker treat the shell
    /// as a new element each time, so it could never work out what actually
    /// changed and would repaint the whole output forever.
    #[cfg(feature = "wpe")]
    pub shell_element_id: smithay::backend::renderer::element::Id,
    /// What changed in the shell's buffer since the last frame.
    ///
    /// Required, not an optimisation. With a stable id the damage tracker
    /// decides whether to redraw by asking the element what changed, and an
    /// element built with `DamageSnapshot::empty()` answers "nothing" for
    /// ever — so the outputs go quiet after the first frame while WebKit
    /// carries on painting into buffers nobody draws.
    #[cfg(feature = "wpe")]
    pub shell_damage: smithay::backend::renderer::utils::DamageBag<
        i32,
        smithay::utils::Buffer,
    >,

    /// The pointer image: the client's own surface where one is set, the
    /// theme's otherwise. Nothing draws a cursor unless this says what.
    pub cursor_status: smithay::input::pointer::CursorImageStatus,
    /// The xcursor theme, loaded on first use.
    pub cursor_theme: crate::cursor::Theme,
    /// Whether the missing-theme warning has been said. Once is a diagnosis;
    /// every frame is a flood.
    pub cursor_warned: bool,

    /// When the shell last moved a window, so a diagnostic capture can wait
    /// for the open animation to finish. Five shell frames is the middle of
    /// it, where the client has not yet processed its configure.
    pub last_layout: Option<std::time::Instant>,

    /// An output whose contents changed but which has no frame in flight.
    ///
    /// Rendering is driven by vblank and vblank stops when nothing is
    /// submitted, so a client that paints while the screen is still has
    /// nothing to wake the loop for it. Without this a window updates only
    /// when something unrelated happens to cause a frame.
    pub needs_render: bool,

    /// wp_color_management_v1. Smithay has no handler for it, so the
    /// implementation is in crate::color_management.
    pub color_management: crate::color_management::ColorManagementState,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    /// wlr-layer-shell: bars, launchers, notification daemons. Not the
    /// shell's business — a layer surface asks for an edge, not a layout.
    pub layer_shell_state: smithay::wayland::shell::wlr_layer::WlrLayerShellState,
    /// Pointer capture, and the relative motion a game reads instead of a
    /// position. Both are needed together: a lock with no relative motion
    /// leaves a game unable to turn at all.
    pub pointer_constraints_state:
        smithay::wayland::pointer_constraints::PointerConstraintsState,
    pub relative_pointer_state:
        smithay::wayland::relative_pointer::RelativePointerManagerState,
    /// ext-session-lock-v1: the screen locker.
    pub session_lock_state: smithay::wayland::session_lock::SessionLockManagerState,
    /// Whether the session is locked. Stays true if the locker dies, because
    /// otherwise killing it would be the way past it.
    pub locked: bool,
    /// When the session was locked, so a locker that never draws can be
    /// noticed rather than leaving a black screen that says nothing.
    pub locked_at: Option<std::time::Instant>,
    pub lock_warned: bool,
    /// One lock surface per output, by output name.
    pub lock_surfaces:
        std::collections::HashMap<String, smithay::wayland::session_lock::LockSurface>,
    /// xdg-activation. A launcher needs the global to exist before it will
    /// draw at all, quite apart from what activation is for.
    pub xdg_activation_state: smithay::wayland::xdg_activation::XdgActivationState,
    /// linux-dmabuf. Created without a global here: the formats a client may
    /// use are the renderer's, and there is no renderer until a backend has
    /// started. See `advertise_dmabuf`.
    pub dmabuf_state: smithay::wayland::dmabuf::DmabufState,
    /// The X11 window manager, once Xwayland has started. Absent until then,
    /// and absent for good if it could not be spawned.
    pub xwm: Option<smithay::xwayland::X11Wm>,
    /// The X display number, for DISPLAY.
    pub xdisplay: Option<u32>,
    /// How Xwayland says which wl_surface belongs to which X window.
    pub xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState,
    /// zxdg_decoration_manager_v1. Held only so the global outlives the
    /// display; every decision it drives is in the handler.
    pub xdg_decoration_state:
        smithay::wayland::shell::xdg::decoration::XdgDecorationState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub seat: Seat<Self>,
}

impl ViewportState {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        socket_path: Option<std::path::PathBuf>,
    ) -> anyhow::Result<Self> {
        let dh = display.handle();
        let loop_handle = event_loop.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let color_management =
            crate::color_management::ColorManagementState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let layer_shell_state =
            smithay::wayland::shell::wlr_layer::WlrLayerShellState::new::<Self>(&dh);
        let pointer_constraints_state =
            smithay::wayland::pointer_constraints::PointerConstraintsState::new::<Self>(&dh);
        let relative_pointer_state =
            smithay::wayland::relative_pointer::RelativePointerManagerState::new::<Self>(&dh);
        let session_lock_state =
            smithay::wayland::session_lock::SessionLockManagerState::new::<Self, _>(
                &dh,
                // Every client may ask. Restricting it to a privileged few is
                // for a compositor that has a notion of privilege; this one
                // does not, and refusing here would only mean no locker works.
                |_| true,
            );
        let xdg_activation_state =
            smithay::wayland::xdg_activation::XdgActivationState::new::<Self>(&dh);
        let dmabuf_state = smithay::wayland::dmabuf::DmabufState::new();
        let xwayland_shell_state =
            smithay::wayland::xwayland_shell::XWaylandShellState::new::<Self>(&dh);
        let xdg_decoration_state =
            smithay::wayland::shell::xdg::decoration::XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);

        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "viewport");
        seat.add_keyboard(Default::default(), 200, 25)?;
        seat.add_pointer();

        let socket_name = Self::init_wayland_listener(display, event_loop);

        // The control socket is named after the Wayland display, so it has to
        // wait until the display exists.
        let path = socket_path.unwrap_or_else(|| {
            Ipc::default_path(socket_name.to_str())
        });
        let ipc = Ipc::new(path, &loop_handle)?;

        Ok(Self {
            start_time: std::time::Instant::now(),
            socket_name,
            display_handle: dh,
            loop_signal: event_loop.get_signal(),
            loop_handle,

            space: Space::default(),
            popups: PopupManager::default(),
            views: Views::new(),
            focused: NO_VIEW,

            ipc,
            shell_announced: false,
            config: Config {
                layout: "tiling".to_owned(),
                // Both true, as in src/main.c:69 — "the empty desktop explains
                // itself until told not to". These set no-logo and no-tutorial
                // on the document when false, and on a desktop with no windows
                // they are the only things there are to draw.
                logo: true,
                tutorial: true,
                bar: None,
                rules: None,
                theme: None,
            },
            shell_url: None,
            output_config: std::collections::HashMap::new(),
            startup: None,
            status: crate::status::Status::default(),
            idle: crate::idle::Idle::default(),
            idle_settings: crate::idle::Settings::default(),
            vt_switching: true,
            server_decorations: true,
            dark_mode: true,
            adaptive_sync: false,
            fallback_url: None,
            // C's default (`src/main.c:54`). The deadline is on the first
            // painted frame, not on the load event.
            load_timeout_ms: 5000,
            logo_held: false,
            overview: false,
            active_output: None,
            udev: None,
            suppressed_keys: Vec::new(),
            bindings: crate::binding::defaults(
                &std::env::var("VIEWPORT_TERMINAL").unwrap_or_else(|_| "foot".to_owned()),
                &std::env::var("VIEWPORT_MENU").unwrap_or_else(|_| "wmenu-run".to_owned()),
                false,
            ),
            #[cfg(feature = "wpe")]
            glib: None,
            #[cfg(feature = "wpe")]
            shell: None,
            #[cfg(feature = "wpe")]
            shell_ping: None,
            #[cfg(feature = "wpe")]
            shell_size: None,
            #[cfg(feature = "wpe")]
            shell_renderer: None,
            #[cfg(feature = "wpe")]
            shell_owned: None,
            #[cfg(feature = "wpe")]
            shell_frames: 0,
            #[cfg(feature = "wpe")]
            shell_element_id: smithay::backend::renderer::element::Id::new(),
            #[cfg(feature = "wpe")]
            shell_damage: Default::default(),

            cursor_status: smithay::input::pointer::CursorImageStatus::default_named(),
            cursor_theme: crate::cursor::Theme::new(),
            cursor_warned: false,
            last_layout: None,
            needs_render: false,

            color_management,
            compositor_state,
            xdg_shell_state,
            layer_shell_state,
            pointer_constraints_state,
            relative_pointer_state,
            session_lock_state,
            locked: false,
            locked_at: None,
            lock_warned: false,
            lock_surfaces: std::collections::HashMap::new(),
            xdg_activation_state,
            dmabuf_state,
            xwm: None,
            xdisplay: None,
            xwayland_shell_state,
            xdg_decoration_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            seat,
        })
    }

    fn init_wayland_listener(
        display: Display<Self>,
        event_loop: &mut EventLoop<'static, Self>,
    ) -> OsString {
        let listening_socket = ListeningSocketSource::new_auto().unwrap();
        let socket_name = listening_socket.socket_name().to_os_string();
        let loop_handle = event_loop.handle();

        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                    .unwrap();
            })
            .expect("failed to init the wayland event source");

        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // Safety: the display is not dropped here.
                    unsafe {
                        display.get_mut().dispatch_clients(state).unwrap();
                    }
                    Ok(PostAction::Continue)
                },
            )
            .unwrap();

        socket_name
    }

    /// What the pointer is over.
    ///
    /// Falls through to nothing when no window is under it, which in the
    /// finished compositor means the shell's own buffer — that is the property
    /// that makes "click went to the titlebar" versus "click went to the app"
    /// need no geometry bookkeeping.
    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        if self.locked {
            // Only the locker may be reached. Its own surface is focused
            // explicitly when it commits; nothing is picked by position,
            // because there is nothing else the pointer may touch.
            return None;
        }
        if self.overview {
            // Every click belongs to the shell while it is drawing miniatures.
            return None;
        }

        // Layer surfaces first where they are in front, and last where they
        // are behind, so a launcher over a window takes the click and a
        // wallpaper client under one does not.
        let output = self
            .space
            .output_under(pos)
            .next()
            .cloned()
            .or_else(|| self.space.outputs().next().cloned());
        let (above, below) = match output.as_ref() {
            Some(output) => {
                let geometry = self.space.output_geometry(output).unwrap_or_default();
                let local = pos - geometry.loc.to_f64();
                let map = smithay::desktop::layer_map_for_output(output);
                let hit = |layer: Option<&smithay::desktop::LayerSurface>| {
                    let layer = layer?;
                    let at = map.layer_geometry(layer)?.loc.to_f64() + geometry.loc.to_f64();
                    layer
                        .surface_under(pos - at, WindowSurfaceType::ALL)
                        .map(|(s, p)| (s, p.to_f64() + at))
                };
                use smithay::wayland::shell::wlr_layer::Layer;
                (
                    hit(map.layer_under(Layer::Overlay, local))
                        .or_else(|| hit(map.layer_under(Layer::Top, local))),
                    hit(map.layer_under(Layer::Bottom, local))
                        .or_else(|| hit(map.layer_under(Layer::Background, local))),
                )
            }
            None => (None, None),
        };

        if above.is_some() {
            return above;
        }
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + location).to_f64()))
            })
            .or(below)
    }

    /// Advertise linux-dmabuf, with the formats this renderer can import.
    ///
    /// After the backend, not before: the format list is the renderer's, and a
    /// global advertising formats nobody can import is worse than none — the
    /// client picks one, hands over a buffer, and finds out at the first frame.
    ///
    /// The feedback names the render node, which is how a client knows which
    /// GPU to allocate on when there is more than one.
    pub fn advertise_dmabuf(
        &mut self,
        render_node: Option<u64>,
        formats: Vec<smithay::backend::allocator::Format>,
    ) {
        use smithay::wayland::dmabuf::DmabufFeedbackBuilder;

        if formats.is_empty() {
            tracing::warn!("the renderer imports no dmabuf format; not advertising linux-dmabuf");
            return;
        }
        let Some(node) = render_node else {
            tracing::warn!("no render node; not advertising linux-dmabuf");
            return;
        };

        let count = formats.len();
        match DmabufFeedbackBuilder::new(node, formats).build() {
            Ok(feedback) => {
                self.dmabuf_state
                    .create_global_with_default_feedback::<Self>(&self.display_handle, &feedback);
                tracing::info!("linux-dmabuf: {count} format/modifier pair(s)");
            }
            Err(e) => tracing::error!("could not build dmabuf feedback: {e}"),
        }
    }

    /// Turn variable refresh on or off for every output that supports it.
    ///
    /// Whole-session rather than per-output, as in C (`src/output.c:315`): the
    /// config key is not under `outputs`, and a display that cannot do it says
    /// so rather than failing the commit.
    pub fn set_adaptive_sync(&mut self, enabled: bool) {
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        for surface in udev.surfaces.values_mut() {
            let result = surface
                .drm_output
                .with_compositor(|compositor| compositor.use_vrr(enabled));
            match result {
                Ok(()) => tracing::info!(
                    "adaptive sync {} on {}",
                    if enabled { "on" } else { "off" },
                    surface.output.name()
                ),
                // Not an error worth stopping for: most panels do not do it,
                // and asking is how you find out.
                Err(e) => tracing::debug!(
                    "adaptive sync unavailable on {}: {e}",
                    surface.output.name()
                ),
            }
        }
    }

    /// Turn every output on or off.
    ///
    /// Blanking is a DRM state change rather than drawing black: a black frame
    /// still lights the panel, and the point is that the monitor sleeps.
    pub fn set_outputs_enabled(&mut self, enabled: bool) {
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        if udev.blanked == !enabled {
            return;
        }
        udev.blanked = !enabled;
        tracing::info!("outputs {}", if enabled { "on" } else { "off" });

        if enabled {
            // Nothing to undo explicitly: `clear` re-enables on the next
            // queued frame. But vblank cannot provide one — nothing has been
            // queued since the screens went off — so the frame has to be asked
            // for.
            for surface in udev.surfaces.values_mut() {
                surface.pending = false;
            }
            self.needs_render = true;
            return;
        }

        for surface in udev.surfaces.values_mut() {
            // DPMS off and every plane disabled, rather than a black frame: a
            // black frame still lights the panel, and the point is that the
            // monitor sleeps.
            if let Err(e) = surface
                .drm_output
                .with_compositor(|compositor| compositor.clear())
            {
                tracing::warn!("could not blank an output: {e}");
            }
            // No frame is in flight now, and none will be until the screens
            // come back.
            surface.pending = false;
        }
    }

    /// Load the fallback page if the shell has not painted in time.
    ///
    /// The deadline is on the first *painted frame*, not on the load event
    /// (`src/web.c:100`). A page that loads and then stalls, or one whose
    /// script throws before it renders, leaves the user staring at a blank
    /// screen — and both are invisible to a load-failed signal.
    #[cfg(feature = "wpe")]
    pub fn check_shell_loaded(&mut self) {
        if self.shell_frames > 0 || self.shell.is_none() {
            return;
        }
        let url = self.fallback_url.clone().unwrap_or_else(|| {
            let here = std::env::current_dir().unwrap_or_default();
            format!("file://{}/data/fallback.html", here.display())
        });
        tracing::error!(
            "the shell painted nothing within {}ms; loading {url}",
            self.load_timeout_ms
        );
        if let Some(shell) = self.shell.as_ref() {
            if let Err(e) = shell.view.load(&url) {
                tracing::error!("the fallback would not load either: {e:#}");
            }
        }
    }

    /// Lay every window out ourselves, because the shell has not.
    ///
    /// Only reached when a window has been waiting for a rectangle longer than
    /// the shell should ever take. Everything it places is marked as placed,
    /// so the watchdog does not fire again for the same windows — and the
    /// moment a real `view.layout` arrives it overrides this.
    pub fn watchdog_fire(&mut self, id: u32) {
        // Answered after all: a shell that is merely slow costs nothing.
        if self.views.get(id).map(|view| view.placed).unwrap_or(true) {
            return;
        }

        tracing::error!(
            "the shell did not place view {id} within {}ms; falling back to a \
             built-in layout. The shell is broken or unreachable.",
            crate::watchdog::TIMEOUT.as_millis()
        );

        let (width, height) = self.layout_size();
        let origin = self
            .space
            .outputs()
            .filter_map(|output| self.space.output_geometry(output))
            .map(|geometry| (geometry.loc.x, geometry.loc.y))
            .min()
            .unwrap_or((0, 0));

        let ids: Vec<u32> = self
            .views
            .iter()
            .filter(|view| view.mapped && view.visible)
            .map(|view| view.id)
            .collect();

        for placed in crate::watchdog::columns(
            &ids,
            (origin.0, origin.1, width as i32, height as i32),
        ) {
            // Through the ordinary layout path, so a window ends up configured
            // and mapped exactly as the shell would have done it.
            crate::apply::apply(
                self,
                viewport_ipc::Request::ViewLayout(viewport_ipc::request::ViewLayout {
                    id: placed.id,
                    box_: viewport_ipc::geometry::PartialBox {
                        x: Some(placed.x),
                        y: Some(placed.y),
                        width: Some(placed.width),
                        height: Some(placed.height),
                    },
                    // No clip: a clip describes the hole the shell drew, and
                    // there is no shell answering.
                    clip: None,
                    scale: None,
                }),
            );
        }
    }

    /// The size of everything, which is what the shell spans.
    ///
    /// Not gated on the web engine: the layout watchdog needs it too, and a
    /// compositor built without a shell still has outputs to lay windows out
    /// across.
    pub fn layout_size(&self) -> (u32, u32) {
        let size = self.space.outputs().fold((0i32, 0i32), |acc, output| {
            match self.space.output_geometry(output) {
                Some(geometry) => (
                    acc.0.max(geometry.loc.x + geometry.size.w),
                    acc.1.max(geometry.loc.y + geometry.size.h),
                ),
                None => acc,
            }
        });
        (size.0.max(0) as u32, size.1.max(0) as u32)
    }

    /// Sample the machine and tell the shell.
    ///
    /// The page cannot do this for itself: it is loaded from file:// or
    /// http://, and neither origin can read /proc. How the numbers are
    /// *displayed* is still entirely the shell's business.
    pub fn status_tick(&mut self) {
        let sample = self.status.sample();
        let event = viewport_ipc::Event::StatusUpdate {
            // -1 rather than absent, which is what the bar tests for.
            cpu: sample.cpu.unwrap_or(-1.0),
            memory: sample.memory.unwrap_or(-1.0),
            load: sample.load[0],
            net_rx: sample.net_rx,
            net_tx: sample.net_tx,
            disk_free: sample.disk_free,
            disk_total: sample.disk_total,
        };
        self.notify(&event);
    }

    /// One idle tick: lock and blank when their deadlines pass.
    pub fn idle_tick(&mut self) {
        if !self.idle_settings.wanted() {
            return;
        }
        let elapsed = self.idle.since.elapsed();
        let actions = self.idle.tick(&self.idle_settings, elapsed);
        if actions.lock {
            match self.idle_settings.lock_command.clone() {
                Some(command) => {
                    tracing::info!("idle for {}s; locking", elapsed.as_secs());
                    crate::input::spawn(&command);
                }
                // Nothing to run. Said once per idle period rather than
                // silently doing nothing, because "lock_after" with no
                // "lock_command" looks like it should work.
                None => tracing::warn!(
                    "idle.lock_after passed but no idle.lock_command is set"
                ),
            }
        }
        if actions.blank {
            tracing::info!("idle for {}s; blanking", elapsed.as_secs());
            self.set_outputs_enabled(false);
        }
    }

    /// Apply the config file's `outputs` block, once the outputs exist.
    ///
    /// Through the same path `output.configure` takes, so the file and the
    /// shell cannot disagree about what a mode change means — and so a
    /// rejected mode is reported the same way whichever asked for it.
    ///
    /// Called after connectors come up rather than at load: an output that is
    /// not plugged in has nothing to configure, and one plugged in later gets
    /// this again.
    pub fn apply_output_config(&mut self) {
        let outputs = std::mem::take(&mut self.output_config);
        for (name, want) in &outputs {
            if self.output_by_name(name).is_none() {
                // Not plugged in. Kept, because it may be later.
                continue;
            }
            let mode = want.mode.as_deref().and_then(|text| {
                let parsed = crate::config::parse_mode(text);
                if parsed.is_none() {
                    tracing::error!("outputs.{name}.mode {text:?} is not WIDTHxHEIGHT[@RATE]");
                }
                parsed
            });
            let request = viewport_ipc::request::OutputConfigure {
                name: name.clone(),
                enabled: None,
                mode: mode.map(|(width, height, refresh)| viewport_ipc::request::ModeRequest {
                    width,
                    height,
                    // Zero means "any rate at this resolution", which is what
                    // a mode string without one asks for.
                    refresh: refresh.unwrap_or(0),
                }),
                scale: want.scale,
                transform: want.transform.as_deref().and_then(parse_transform),
                adaptive_sync: None,
                x: want.x,
                y: want.y,
            };
            tracing::info!("configuring {name} from the config file");
            crate::apply::apply(self, viewport_ipc::Request::OutputConfigure(request));

            // HDR is its own message, because turning it on is a colour change
            // rather than a mode change and the two are answered differently.
            if let Some(hdr) = want.hdr {
                crate::apply::apply(
                    self,
                    viewport_ipc::Request::OutputHdr {
                        name: Some(name.clone()),
                        enabled: Some(hdr),
                    },
                );
            }
        }
        self.output_config = outputs;
    }

    /// What an output should show, worked out without a renderer.
    ///
    /// Everything the backend would otherwise have to reach into this state
    /// for while its renderer is borrowed. The two backends share it, which is
    /// what stops the nested one drifting into showing something different
    /// from the real thing.
    pub fn frame_for(&mut self, output: &Output) -> crate::render::Frame {
        use smithay::wayland::shell::wlr_layer::Layer;

        let Some(output_geometry) = self.space.output_geometry(output) else {
            return crate::render::Frame::default();
        };
        let scale = output.current_scale().fractional_scale();

        // Layer surfaces, split by whether they sit above the windows or
        // below them, in output-local physical coordinates.
        let (mut layers_above, mut layers_below) = (Vec::new(), Vec::new());
        {
            let map = smithay::desktop::layer_map_for_output(output);
            for layer in map.layers() {
                let Some(geometry) = map.layer_geometry(layer) else {
                    continue;
                };
                let location = geometry.loc.to_f64().to_physical(scale).to_i32_round();
                let entry = (layer.clone(), location);
                match layer.layer() {
                    Layer::Overlay | Layer::Top => layers_above.push(entry),
                    Layer::Background | Layer::Bottom => layers_below.push(entry),
                }
            }
        }

        let windows: Vec<_> = self
            .space
            .elements()
            .filter_map(|window| {
                let layout = self.space.element_geometry(window)?;
                // Off this output entirely: drawing it would cost a texture
                // bind for something wholly clipped away.
                if !output_geometry.overlaps(layout) {
                    return None;
                }
                let clip = window
                    .toplevel()
                    .map(|toplevel| toplevel.wl_surface().clone())
                    .and_then(|surface| self.views.find_by_surface(&surface))
                    .and_then(|view| view.clip)
                    .map(|clip| {
                        Rectangle::<i32, Logical>::new(
                            (clip.x, clip.y).into(),
                            (clip.width, clip.height).into(),
                        )
                    });
                let (location, clip) = crate::render::window_placement(
                    window,
                    layout,
                    output_geometry,
                    clip,
                    scale,
                );
                Some((window.clone(), location, clip))
            })
            .collect();

        let cursor = self.cursor_for(output, output_geometry, scale);

        #[cfg(feature = "wpe")]
        let shell = self.shell_owned.as_ref().map(|(buffer, _)| crate::render::Shell {
            buffer: buffer.clone(),
            // Negative of the output's position: the shell is one buffer
            // across the whole layout.
            location: (
                -output_geometry.loc.x as f64 * scale,
                -output_geometry.loc.y as f64 * scale,
            )
                .into(),
            damage: self.shell_damage.snapshot(),
            id: self.shell_element_id.clone(),
        });
        #[cfg(not(feature = "wpe"))]
        let shell = None;

        crate::render::Frame {
            layers_above,
            windows,
            layers_below,
            shell,
            cursor,
            scale,
            lock: self
                .lock_surfaces
                .get(&output.name())
                // A locker that exited leaves its surfaces behind until the
                // next housekeeping tick; drawing one is drawing nothing.
                .filter(|lock| smithay::utils::IsAlive::alive(lock.wl_surface()))
                .map(|lock| lock.wl_surface().clone()),
            locked_blank: self.locked,
        }
    }

    /// The pointer image for an output, resolved but not imported.
    fn cursor_for(
        &mut self,
        output: &Output,
        output_geometry: Rectangle<i32, Logical>,
        scale: f64,
    ) -> crate::render::Cursor {
        use smithay::input::pointer::CursorImageStatus;

        let _ = output;
        let Some(pointer) = self.seat.get_pointer() else {
            return crate::render::Cursor::Hidden;
        };
        let at = pointer.current_location();
        if !output_geometry.to_f64().contains(at) {
            return crate::render::Cursor::Hidden;
        }
        let local = (at - output_geometry.loc.to_f64()).to_physical(scale);

        match self.cursor_status.clone() {
            CursorImageStatus::Hidden => crate::render::Cursor::Hidden,
            CursorImageStatus::Surface(surface) => {
                let hotspot = smithay::wayland::compositor::with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<std::sync::Mutex<smithay::input::pointer::CursorImageAttributes>>()
                        .map(|attrs| attrs.lock().unwrap().hotspot)
                        .unwrap_or_default()
                });
                // The surface is drawn at the pointer minus its hotspot, and
                // `build` subtracts the hotspot — so this carries the pointer
                // position folded in.
                let at = local.to_i32_round();
                crate::render::Cursor::Surface(
                    surface,
                    hotspot.to_f64().to_physical(scale).to_i32_round() - at,
                )
            }
            CursorImageStatus::Named(shape) => {
                let millis = self.start_time.elapsed().as_millis() as u32;
                match self.cursor_theme.image(shape.name(), scale.ceil() as i32, millis) {
                    Some((buffer, hotspot)) => {
                        crate::render::Cursor::Image(buffer, local.to_i32_round() - hotspot)
                    }
                    None => {
                        if !self.cursor_warned {
                            self.cursor_warned = true;
                            tracing::warn!(
                                "no xcursor image for {:?}; set XCURSOR_THEME to a theme that is installed",
                                shape.name()
                            );
                        }
                        crate::render::Cursor::Hidden
                    }
                }
            }
        }
    }

    /// Start Xwayland, so X11 applications can connect.
    ///
    /// Lazily is tempting — a session with no X client never needs it — but
    /// DISPLAY has to be in the environment before anything is spawned, and
    /// the whole point is that an X program started from a menu just works.
    pub fn start_xwayland(&mut self, loop_handle: &LoopHandle<'static, Self>) {
        use smithay::xwayland::{XWayland, XWaylandEvent};

        let (xwayland, client) = match XWayland::spawn(
            &self.display_handle,
            None,
            std::iter::empty::<(String, String)>(),
            std::iter::empty::<String>(),
            true,
            std::process::Stdio::null(),
            std::process::Stdio::null(),
            |_| (),
        ) {
            Ok(pair) => pair,
            // Not fatal. A compositor that will not start because Xwayland is
            // not installed is worse than one without X11 support.
            Err(e) => {
                tracing::warn!("Xwayland did not start, so X11 clients cannot connect: {e}");
                return;
            }
        };

        let display_handle = self.display_handle.clone();
        let handle = loop_handle.clone();
        let inserted = loop_handle.insert_source(xwayland, move |event, _, state| match event {
            XWaylandEvent::Ready { x11_socket, display_number } => {
                match X11Wm::start_wm(handle.clone(), &display_handle, x11_socket, client.clone()) {
                    Ok(wm) => {
                        state.xwm = Some(wm);
                        state.xdisplay = Some(display_number);
                        // Anything spawned from here on finds an X server.
                        unsafe { std::env::set_var("DISPLAY", format!(":{display_number}")) };
                        tracing::info!("Xwayland ready on :{display_number}");
                    }
                    Err(e) => tracing::error!("could not attach the X11 window manager: {e}"),
                }
            }
            XWaylandEvent::Error => {
                tracing::warn!("Xwayland crashed on startup; X11 clients cannot connect");
            }
        });
        if let Err(e) = inserted {
            tracing::error!("inserting the Xwayland source: {e}");
        }
    }

    /// The output a new window should be told it is on.
    pub fn output_for_new_view(&self) -> String {
        self.active_output
            .clone()
            .or_else(|| self.space.outputs().next().map(|o| o.name()))
            .unwrap_or_default()
    }

    pub fn output_by_name(&self, name: &str) -> Option<Output> {
        self.space.outputs().find(|o| o.name() == name).cloned()
    }

    /// Announce every mapped window, as a replay.
    ///
    /// This is how a shell that reloaded rebuilds its tree: the windows are not
    /// new, so `replay` is set and the shell restores them into the slots they
    /// left rather than appending them wherever there is room.
    pub fn notify_views(&mut self) {
        let output = self.output_for_new_view();
        let events: Vec<Event> = self
            .views
            .iter()
            .filter(|v| v.mapped)
            .map(|v| Event::ViewAdded(v.added(output.clone(), true)))
            .collect();
        for event in events {
            self.notify(&event);
        }
    }

    pub fn notify_config(&mut self) {
        // Config parsing is not ported yet; these are the C build's defaults
        // (`src/main.c:61`).
        //
        // logo and tutorial are true there — "the empty desktop explains
        // itself until told not to". Sending false is not a smaller default,
        // it sets no-logo and no-tutorial on the document, and on a desktop
        // with no windows those two are the only things there are to draw. It
        // leaves the wallpaper and nothing else, which is what three runs of
        // "the right display is grey" actually were.
        let event = Event::Config(self.config.clone());
        self.notify(&event);
    }

    /// Apply a config file over the built-in defaults.
    ///
    /// Only what the file contains: a key left out never resets something a
    /// flag or an earlier load set, which is what makes a reload safe
    /// (`src/config.c:400`).
    pub fn apply_config(&mut self, file: crate::config::File) {
        if let Some(layout) = file.layout {
            self.config.layout = layout;
        }
        if let Some(logo) = file.logo {
            self.config.logo = logo;
        }
        if let Some(tutorial) = file.tutorial {
            self.config.tutorial = tutorial;
        }
        if let Some(bar) = file.bar {
            self.config.bar = Some(bar);
        }
        if file.rules.is_some() {
            self.config.rules = file.rules;
        }
        if file.theme.is_some() {
            self.config.theme = file.theme;
        }
        if let Some(url) = file.url {
            self.shell_url = Some(url);
        }
        if !file.outputs.is_empty() {
            self.output_config = file.outputs;
        }
        // Run after the compositor is up, so it reaches whatever it names.
        if let Some(command) = file.startup.as_deref() {
            self.startup = Some(command.to_owned());
        }
        if let Some(url) = file.fallback {
            self.fallback_url = Some(url);
        }
        if let Some(ms) = file.timeout_ms {
            self.load_timeout_ms = ms.max(0) as u64;
        }
        if let Some(allowed) = file.vt_switching {
            self.vt_switching = allowed;
        }
        if let Some(dark) = file.dark_mode {
            self.dark_mode = dark;
        }
        if let Some(vrr) = file.adaptive_sync {
            self.adaptive_sync = vrr;
        }
        if let Some(mode) = file.decorations.as_deref() {
            // "client" hands the frame back; anything else, including a value
            // nobody recognises, keeps it here (`src/config.c:315`).
            self.server_decorations = mode != "client";
        }
        if file.idle != crate::config::IdleConfig::default() {
            self.idle_settings = crate::idle::Settings {
                lock_after: file.idle.lock_after,
                blank_after: file.idle.blank_after,
                lock_command: file.idle.lock_command,
            };
        }

        // The cursor theme. The xcursor loader reads the environment, which is
        // also how every toolkit resolves it — so setting it here is what makes
        // the compositor's pointer and a GTK application's agree.
        if let Some(theme) = file.cursor.theme.as_deref() {
            unsafe { std::env::set_var("XCURSOR_THEME", theme) };
        }
        if let Some(size) = file.cursor.size {
            unsafe { std::env::set_var("XCURSOR_SIZE", size.to_string()) };
        }
        if file.cursor != crate::config::CursorConfig::default() {
            self.cursor_theme = crate::cursor::Theme::new();
        }

        // The keymap, if the file names one. Replacing the keyboard is how
        // this is set — there is no way to change the layout of one that
        // already exists — so it happens before any client has seen a seat.
        let keyboard = &file.keyboard;
        if keyboard != &crate::config::KeyboardConfig::default() {
            let xkb = smithay::input::keyboard::XkbConfig {
                layout: keyboard.layout.as_deref().unwrap_or(""),
                variant: keyboard.variant.as_deref().unwrap_or(""),
                options: keyboard.options.clone(),
                ..Default::default()
            };
            // C's defaults, which are sway's (`src/main.c`): 25 a second after
            // 200ms.
            let delay = keyboard.repeat_delay.unwrap_or(200);
            let rate = keyboard.repeat_rate.unwrap_or(25);
            match self.seat.add_keyboard(xkb, delay, rate) {
                Ok(_) => tracing::info!(
                    "keymap {:?}{}, repeat {rate}/s after {delay}ms",
                    keyboard.layout.as_deref().unwrap_or("(default)"),
                    keyboard
                        .variant
                        .as_deref()
                        .map(|v| format!(" {v}"))
                        .unwrap_or_default(),
                ),
                // Naming it matters: an unknown layout otherwise leaves the
                // built-in one in place and looks like the config was ignored.
                Err(e) => tracing::error!(
                    "keymap {:?} was refused, keeping the current one: {e}",
                    keyboard.layout.as_deref().unwrap_or("(default)")
                ),
            }
        }

        // Bindings last, because whether the defaults are there at all depends
        // on the file. Presence of "binds" means "this is the whole keymap",
        // so an empty one asks for none.
        let terminal = file
            .terminal
            .or_else(|| std::env::var("VIEWPORT_TERMINAL").ok())
            .unwrap_or_else(|| "foot".to_owned());
        let menu = file
            .menu
            .or_else(|| std::env::var("VIEWPORT_MENU").ok())
            .unwrap_or_else(|| "wmenu-run".to_owned());
        let scrolling = self.config.layout == "scrolling";

        let mut bindings = Vec::new();
        // Overrides go in front: bindings are matched first-wins, so a chord
        // the file claims shadows the default without the default needing to
        // be removed.
        if let Some(over) = file.binds_override.as_ref() {
            bindings.extend(
                crate::config::bind_specs(over)
                    .iter()
                    .filter_map(|spec| crate::binding::parse(spec)),
            );
        }
        match file.binds.as_ref() {
            Some(binds) => bindings.extend(
                crate::config::bind_specs(binds)
                    .iter()
                    .filter_map(|spec| crate::binding::parse(spec)),
            ),
            None => bindings.extend(crate::binding::defaults(&terminal, &menu, scrolling)),
        }
        self.bindings = bindings;
    }

    pub fn notify_output_layout(&mut self) {
        let outputs: Vec<OutputInfo> = self
            .space
            .outputs()
            .map(|output| {
                let geometry = self.space.output_geometry(output).unwrap_or_default();
                let usable = self.usable_area(output);
                let props = output.physical_properties();
                let current = output.current_mode();
                OutputInfo {
                    name: output.name(),
                    // Never null: the shell concatenates these without
                    // guarding (`src/ipc.c:704`).
                    make: props.make,
                    model: props.model,
                    serial: String::new(),
                    enabled: true,
                    x: geometry.loc.x,
                    y: geometry.loc.y,
                    width: geometry.size.w,
                    height: geometry.size.h,
                    // What is left after exclusive zones. A bar that reserved
                    // the top of the screen has taken that space away from the
                    // shell, which is the only thing that places windows.
                    usable_x: usable.loc.x,
                    usable_y: usable.loc.y,
                    usable_width: usable.size.w,
                    usable_height: usable.size.h,
                    hdr: false,
                    hdr_capable: false,
                    scale: output.current_scale().fractional_scale(),
                    transform: Transform::Normal,
                    modes: output
                        .modes()
                        .into_iter()
                        .map(|mode| viewport_ipc::event::Mode {
                            width: mode.size.w,
                            height: mode.size.h,
                            refresh: mode.refresh,
                            preferred: output.preferred_mode() == Some(mode),
                            current: current == Some(mode),
                        })
                        .collect(),
                }
            })
            .collect();

        // The shell is one buffer across the whole layout, so a change to the
        // layout is a change to its size. Without this it keeps whatever size
        // it had when it started: a monitor plugged in later, or a nested
        // window resized, leaves the rest of the screen on the clear colour.
        #[cfg(feature = "wpe")]
        self.resize_shell();

        let event = Event::OutputLayout { outputs };
        self.notify(&event);
    }

    /// Send an event to everything listening: the socket clients and the
    /// shell.
    ///
    /// The shell is not a socket client — it is spoken to through JavaScript —
    /// so anything that only broadcasts on the socket is invisible to the one
    /// thing that draws the desktop.
    pub fn notify(&mut self, event: &Event) {
        self.ipc.broadcast(event);
        #[cfg(feature = "wpe")]
        if let Some(shell) = self.shell.as_ref() {
            // Both directions, because a message that is sent and one that
            // arrives look the same from here and only one of them explains a
            // shell that draws its wallpaper and nothing else.
            tracing::debug!("to shell: {event:?}");
            if let Err(e) = shell.post(event) {
                tracing::warn!("could not post to the shell: {e:#}");
            }
        }
    }

    /// Draw any output that has something new to show.
    ///
    /// Called from the outer loop rather than from wherever the change
    /// happened, so a commit that touches five subsurfaces costs one frame
    /// instead of five.
    pub fn render_if_needed(&mut self) {
        if !std::mem::take(&mut self.needs_render) {
            return;
        }
        // Drawing while the screens are off would queue a frame, and a queued
        // frame is what turns them back on.
        if self.udev.as_ref().map(|udev| udev.blanked).unwrap_or(false) {
            return;
        }
        // Nested has no crtcs; that backend redraws continuously and takes
        // what it needs from the same shared frame description.
        let crtcs: Vec<_> = self
            .udev
            .as_ref()
            .map(|udev| udev.surfaces.keys().copied().collect())
            .unwrap_or_default();
        for crtc in crtcs {
            self.render(crtc);
        }
    }

    pub fn notify_focus(&mut self, id: u32) {
        self.focused = id;
        let event = Event::ViewFocused { id };
        self.notify(&event);
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

impl ViewportState {
    /// Stop the compositor.
    ///
    /// calloop's signal ends its own dispatch, which under the web engine is
    /// only the inner loop — so the outer GLib loop has to be told as well or
    /// quitting does nothing visible.
    pub fn shutdown(&mut self) {
        self.loop_signal.stop();
        #[cfg(feature = "wpe")]
        if let Some(glib) = self.glib {
            glib.quit();
        }
    }
}

#[cfg(feature = "wpe")]
impl ViewportState {
    /// Start the shell on the same GPU the renderer uses.
    ///
    /// The formats offered to WebKit are the renderer's own importable set. A
    /// format the compositor cannot import produces a shell that never
    /// appears rather than an error, so asking the renderer is the only
    /// honest way to build that list.
    pub fn start_shell(
        &mut self,
        card: &smithay::backend::drm::DrmNode,
        render: &smithay::backend::drm::DrmNode,
    ) -> anyhow::Result<()> {
        use smithay::backend::renderer::ImportDma as _;

        // A renderer of the compositor's own, on the render node, for copying
        // WebKit's frames into buffers it owns.
        //
        // Not the backend's: the copy is about owning the buffer rather than
        // about the output, and nesting under another compositor has no DRM
        // renderer at all. Both backends then import the copy into whatever
        // they draw with, which is what lets the nested one show the desktop.
        if self.shell_renderer.is_none() {
            let instance = smithay::backend::vulkan::Instance::new(
                smithay::backend::vulkan::version::Version::VERSION_1_3,
                None,
            )
            .map_err(|e| anyhow::anyhow!("creating a vulkan instance for the shell: {e}"))?;
            let device = viewport_vulkan::Device::for_node(&instance, render)
                .map_err(|e| anyhow::anyhow!("opening a vulkan device for the shell: {e}"))?;
            // With an allocator: the copy needs somewhere of its own to draw
            // into, and a renderer without one cannot make an offscreen at
            // all — which presents as "no image to copy the shell's frame
            // into" on the first frame.
            //
            // The render node opens directly rather than through the session:
            // it needs no DRM master, which is the whole difference between it
            // and the card node.
            let path = render
                .dev_path()
                .ok_or_else(|| anyhow::anyhow!("the render node has no device path"))?;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|e| anyhow::anyhow!("opening {} for the shell: {e}", path.display()))?;
            let gbm = smithay::backend::allocator::gbm::GbmDevice::new(file)
                .map_err(|e| anyhow::anyhow!("creating a gbm device for the shell: {e}"))?;
            let allocator = smithay::backend::allocator::gbm::GbmAllocator::new(
                gbm,
                smithay::backend::allocator::gbm::GbmBufferFlags::RENDERING,
            );
            let renderer = viewport_vulkan::VulkanRenderer::with_allocator(&device, allocator)
                .map_err(|e| anyhow::anyhow!("creating a vulkan renderer for the shell: {e}"))?;
            self.shell_renderer = Some(renderer);
        }

        let formats: Vec<(u32, u64)> = self
            .shell_renderer
            .as_ref()
            .expect("just created")
            .dmabuf_formats()
            .iter()
            .map(|format| (format.code as u32, u64::from(format.modifier)))
            .collect();
        anyhow::ensure!(!formats.is_empty(), "the renderer imports no dmabuf format");

        let (Some(card_path), Some(render_path)) = (card.dev_path(), render.dev_path()) else {
            anyhow::bail!("the drm nodes have no device paths");
        };

        // Where the shell lives: the config file's "url", then the
        // environment, then the copy in the source tree.
        let url = self
            .shell_url
            .clone()
            .or_else(|| std::env::var("VIEWPORT_SHELL_URL").ok())
            .unwrap_or_else(|| {
                let here = std::env::current_dir().unwrap_or_default();
                format!("file://{}/data/shell/index.html", here.display())
            });
        let console = std::env::var("VIEWPORT_LOG")
            .map(|level| level.contains("debug") || level.contains("trace"))
            .unwrap_or(false);

        let size = self.layout_size();
        anyhow::ensure!(
            size.0 > 0 && size.1 > 0,
            "the shell needs an output to size itself against"
        );

        tracing::info!("starting the shell at {url}, {}x{}", size.0, size.1);
        let shell = crate::shell::Shell::start(
            &card_path,
            &render_path,
            &formats,
            size,
            &url,
            console,
        )?;
        if let Some(ping) = self.shell_ping.clone() {
            shell.wake_with(ping);
        }
        self.shell = Some(shell);
        Ok(())
    }
}

#[cfg(feature = "wpe")]
impl ViewportState {
    /// Import whatever the shell last painted, as a texture.
    ///
    /// The imported texture is cached: WebKit paints only when something
    /// changed, so most frames reuse the previous one, and re-importing a
    /// buffer that has not changed would mean a vkCreateImage per output per
    /// frame.
    ///
    /// The presented frame is acknowledged here rather than after the commit.
    /// That is a simplification — strictly WebKit should be released once the
    /// pixels are on screen — and it means the engine may run one frame ahead
    /// of the display.
    pub fn import_shell_frame(&mut self) {
        use smithay::backend::allocator::Buffer as _;
        use smithay::backend::renderer::ImportDma as _;

        if let Some(pending) = self.shell.as_ref().and_then(|shell| shell.take_frame()) {
            let imported = self
                .shell_renderer
                .as_mut()
                .map(|renderer| renderer.import_dmabuf(&pending.buffer, None));

            match imported {
                Some(Ok(texture)) => {
                    // Once. "The shell did not appear" has two causes that
                    // look identical in the log otherwise: WebKit never
                    // painted, or it painted and the frame was not drawn.
                    if self.shell_owned.is_none() {
                        tracing::info!(
                            "first shell frame imported, {}x{}",
                            pending.buffer.width(),
                            pending.buffer.height()
                        );
                    }
                    // Once, before anything else can have touched it. What
                    // WebKit actually painted is the one thing the log cannot
                    // say, and it is the difference between an empty right
                    // half and a right half put on screen wrongly.
                    if let (Some(path), Some(udev)) =
                        (crate::dump::target(), self.udev.as_mut())
                    {
                        if self.shell_owned.is_none() {
                            if let Err(e) =
                                crate::dump::shell_frame(&mut udev.renderer, &texture, &path)
                            {
                                tracing::error!("could not dump the shell's frame: {e:#}");
                            }
                        }
                    }
                    // The whole buffer, because WebKit's per-frame damage
                    // rectangles are not carried across the shim. Redrawing
                    // more than changed costs a composite; reporting none at
                    // all stops the output.
                    self.shell_damage.add([smithay::utils::Rectangle::from_size(
                        (pending.buffer.width() as i32, pending.buffer.height() as i32).into(),
                    )]);
                    // Into an image of our own, because the buffer goes back
                    // to WebKit below and WebKit will paint into it again.
                    // Sampling it after that is reading the frame the engine
                    // is drawing, which alternates with whatever it drew last
                    // — a picture that changes without the compositor asking,
                    // which is what flicker is.
                    let size: smithay::utils::Size<i32, smithay::utils::Physical> =
                        (pending.buffer.width() as i32, pending.buffer.height() as i32).into();
                    let owned = match self.shell_owned.take() {
                        Some((buffer, at)) if at == size => Some((buffer, at)),
                        // First frame, or the layout changed under it.
                        _ => self
                            .shell_renderer
                            .as_mut()
                            .and_then(|renderer| crate::dump::owned_image(renderer, size).ok())
                            .map(|buffer| (buffer, size)),
                    };
                    match owned {
                        Some((mut buffer, at)) => {
                            let copied = self.shell_renderer.as_mut().map(|renderer| {
                                crate::dump::copy_texture(renderer, &texture, &mut buffer, at)
                            });
                            if let Some(Err(e)) = copied {
                                tracing::error!("could not copy the shell's frame: {e:#}");
                            }
                            // Whichever renderer draws this output imports it
                            // itself — see `render::build`.
                            self.shell_owned = Some((buffer, at));
                        }
                        None => tracing::error!("no image to copy the shell's frame into"),
                    }
                }
                Some(Err(e)) => tracing::error!("could not import the shell's frame: {e}"),
                None => {}
            }

            if let Some(shell) = self.shell.as_ref() {
                // Both, immediately, and in this order.
                //
                // Acknowledging advances WebKit's frame clock; releasing puts
                // the buffer back in its pool. Holding the buffer until the
                // next frame arrives sounds safer and deadlocks instead:
                // WebKit needs a free buffer to paint the next frame, so the
                // frame that would trigger the release can never be painted
                // and the shell stops dead after exactly one.
                //
                // Releasing straight away is safe because the frame has been
                // copied into a buffer of the compositor's own just above. A
                // dup'd fd would not have been enough: it is the same memory,
                // so WebKit would paint into the picture on screen.
                shell.frame_done(&pending.token);
                shell.frame_release(pending.token);
                self.shell_frames += 1;
                tracing::debug!("shell frame {} released", self.shell_frames);
            }
        }

        if let Some(shell) = self.shell.as_ref() {
            // Frames the mailbox threw away before anything drew them.
            for token in shell.take_stale() {
                shell.frame_release(token);
            }
        }

    }


    /// Tell the shell how big it is.
    ///
    /// WebKit paints nothing into a view with no size, so without this the
    /// page loads, runs, talks to the compositor — and never produces a frame.
    pub fn resize_shell(&mut self) {
        let size = self.layout_size();
        if size.0 == 0 || size.1 == 0 {
            return;
        }
        // Only on a change: this is called from notify_output_layout, which
        // runs for anything that touches the layout — including a layer
        // surface arriving — and telling WebKit to resize to the size it
        // already has costs a full repaint.
        if self.shell_size == Some(size) {
            return;
        }
        self.shell_size = Some(size);
        if let Some(shell) = self.shell.as_ref() {
            tracing::info!("shell size {}x{}", size.0, size.1);
            shell.display.resize(size.0, size.1);
        }
    }
}

/// A transform by the name the config file uses, which is sway's.
fn parse_transform(text: &str) -> Option<Transform> {
    match text {
        "normal" | "0" => Some(Transform::Normal),
        "90" => Some(Transform::_90),
        "180" => Some(Transform::_180),
        "270" => Some(Transform::_270),
        "flipped" => Some(Transform::Flipped),
        "flipped-90" => Some(Transform::Flipped90),
        "flipped-180" => Some(Transform::Flipped180),
        "flipped-270" => Some(Transform::Flipped270),
        _ => None,
    }
}
