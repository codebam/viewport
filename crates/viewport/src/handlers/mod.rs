// SPDX-License-Identifier: GPL-3.0-or-later
//
// Smithay protocol handler implementations.
//
// Adapted from smithay's `smallvil` example, which is MIT-licensed; this crate
// is GPL-3.0-or-later, which MIT permits.

mod compositor;
mod xdg_shell;

pub mod layer_shell;
pub mod session_lock;
pub mod xwayland;

use smithay::input::dnd::{DnDGrab, DndGrabHandler, GrabType, Source};
use smithay::input::pointer::Focus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::Serial;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    set_data_device_focus, DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;

use crate::state::ViewportState;

impl SeatHandler for ViewportState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    /// A client set its own pointer image, or asked for a named one.
    ///
    /// Kept rather than acted on: what is drawn is decided at render time,
    /// because the same status has to produce a different image on an output
    /// with a different scale.
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: smithay::input::pointer::CursorImageStatus) {
        self.cursor_status = image;
        // The pointer changing shape is a visible change with no other reason
        // to draw a frame behind it.
        self.needs_render = true;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}

impl SelectionHandler for ViewportState {
    type SelectionUserData = ();
}

impl DataDeviceHandler for ViewportState {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl DndGrabHandler for ViewportState {}

impl WaylandDndGrabHandler for ViewportState {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        _icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        match type_ {
            GrabType::Pointer => {
                let Some(pointer) = seat.get_pointer() else {
                    source.cancel();
                    return;
                };
                let Some(start_data) = pointer.grab_start_data() else {
                    source.cancel();
                    return;
                };
                let grab = DnDGrab::new_pointer(&self.display_handle, start_data, source, seat);
                pointer.set_grab(self, grab, serial, Focus::Keep);
            }
            // Touch is not wired up yet.
            GrabType::Touch => source.cancel(),
        }
    }
}

/// Tablet tools, which cursor-shape requires whether or not there is a tablet.
///
/// A tool can name its own cursor exactly as a pointer can, so the protocol's
/// dispatch asks for this. There is no tablet support yet, so the focus type is
/// the same surface every other input uses and a tool setting an image is
/// ignored — the alternative is not advertising cursor-shape at all, which
/// costs every ordinary client its named cursors.
impl smithay::input::tablet::TabletSeatHandler for ViewportState {
    type ToolFocus = WlSurface;
}

impl OutputHandler for ViewportState {}

impl crate::screencopy::ScreencopyHandler for ViewportState {
    fn screencopy_state(&mut self) -> &mut crate::screencopy::ScreencopyState {
        &mut self.screencopy_state
    }

    fn queue_copy(
        &mut self,
        frame: &smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        state: &crate::screencopy::FrameState,
        buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
        with_damage: bool,
    ) -> Result<(), String> {
        self.pending_copies.push(crate::state::PendingCopy {
            frame: frame.clone(),
            buffer: buffer.clone(),
            output: state.output.clone(),
            region: state.region,
            overlay_cursor: state.overlay_cursor,
            with_damage,
        });
        // The copy happens when the output is next drawn, so something has to
        // ask for a draw. An idle desktop draws nothing, and a screenshot of
        // an idle desktop is the ordinary case.
        self.needs_render = true;
        Ok(())
    }
}

/// An input method's own surfaces: the candidate list a Japanese or Chinese
/// method draws while a word is being composed.
///
/// They are popups, tracked in the same manager as an application's menus,
/// which is what puts them on screen — `Window::render_elements` draws the
/// popups belonging to a surface along with it.
impl smithay::wayland::input_method::InputMethodHandler for ViewportState {
    fn new_popup(&mut self, surface: smithay::wayland::input_method::PopupSurface) {
        if let Err(e) = self
            .popups
            .track_popup(smithay::desktop::PopupKind::from(surface))
        {
            tracing::warn!("could not track an input method popup: {e}");
        }
    }

    fn popup_repositioned(&mut self, _surface: smithay::wayland::input_method::PopupSurface) {}

    fn dismiss_popup(&mut self, surface: smithay::wayland::input_method::PopupSurface) {
        let Some(parent) = surface.get_parent().map(|parent| parent.surface.clone()) else {
            return;
        };
        let _ = smithay::desktop::PopupManager::dismiss_popup(
            &parent,
            &smithay::desktop::PopupKind::from(surface),
        );
    }

    /// Where the text being composed is, so the candidate list can sit under
    /// it rather than at the corner of the screen.
    fn parent_geometry(
        &self,
        parent: &WlSurface,
    ) -> smithay::utils::Rectangle<i32, smithay::utils::Logical> {
        use smithay::wayland::seat::WaylandFocus as _;
        self.space
            .elements()
            .find_map(|window| {
                (window.wl_surface().as_deref() == Some(parent)).then(|| window.geometry())
            })
            .unwrap_or_default()
    }
}

/// A client asking to be given the compositor's own chords.
///
/// Granted on the spot, and only ever while that surface has the keyboard:
/// the inhibitor is per surface, so a virtual machine takes Mod4 while it is
/// focused and gives it back the moment focus leaves. Asking the user first is
/// what a compositor with a notification and a policy would do, and there is
/// no policy here to ask about — refusing outright would mean no VM or remote
/// desktop can ever be driven from inside.
impl smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitHandler
    for ViewportState
{
    fn keyboard_shortcuts_inhibit_state(
        &mut self,
    ) -> &mut smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(
        &mut self,
        inhibitor: smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitor,
    ) {
        use smithay::utils::IsAlive as _;

        // Dead ones first: a client that went away leaves its inhibitor
        // behind, and the list is walked on every key press.
        self.shortcut_inhibitors
            .retain(|existing| existing.wl_surface().alive());
        inhibitor.activate();
        self.shortcut_inhibitors.push(inhibitor);
    }
}

/// Acting on the window list from outside: `rofi -show window`, wlrctl, a
/// taskbar with a click-to-focus list.
impl crate::foreign_toplevel::ForeignToplevelHandler for ViewportState {
    fn foreign_toplevel_state(&mut self) -> &mut crate::foreign_toplevel::ForeignToplevelState {
        &mut self.foreign_management_state
    }

    fn activate_toplevel(&mut self, id: u32) {
        // Only a window that is actually on screen, as in `src/foreign.c:53`:
        // focusing one the shell has parked on another workspace would move
        // the keyboard somewhere the user cannot see.
        if self.views.get(id).map(|view| view.mapped).unwrap_or(false) {
            crate::apply::focus_view(self, id);
        }
    }

    fn close_toplevel(&mut self, id: u32) {
        if let Some(toplevel) = self.views.get(id).and_then(|view| view.window.toplevel()) {
            toplevel.send_close();
        }
    }

    fn fullscreen_toplevel(&mut self, id: u32, fullscreen: bool) {
        // The shell owns the layout, so this goes there and comes back as an
        // ordinary state change (`src/foreign.c:71`). The same command C
        // sends, argument for argument (`src/ipc.c:582`), because it is the
        // shell's own vocabulary on the other end.
        self.notify(&viewport_ipc::Event::ShellCommand {
            command: "window.fullscreen.set".to_owned(),
            args: vec![id.to_string(), u8::from(fullscreen).to_string()],
        });
    }
}

impl smithay::wayland::drm_syncobj::DrmSyncobjHandler for ViewportState {
    fn drm_syncobj_state(
        &mut self,
    ) -> Option<&mut smithay::wayland::drm_syncobj::DrmSyncobjState> {
        self.syncobj_state.as_mut()
    }
}

/// The standardised capture protocols, which is what a current
/// xdg-desktop-portal reaches for before wlr-screencopy.
///
/// A source is a thing that can be captured; this compositor offers outputs
/// and nothing else. Toplevel capture would need a window's own contents
/// composited apart from the desktop, and the shell draws window frames
/// around them — a captured window without its frame is not what the picker
/// showed.
impl smithay::wayland::image_capture_source::ImageCaptureSourceHandler for ViewportState {
    fn source_destroyed(
        &mut self,
        source: smithay::wayland::image_capture_source::ImageCaptureSource,
    ) {
        self.capture_sources.retain(|held| held != &source);
    }
}

impl smithay::wayland::image_capture_source::OutputCaptureSourceHandler for ViewportState {
    fn output_capture_source_state(
        &mut self,
    ) -> &mut smithay::wayland::image_capture_source::OutputCaptureSourceState {
        &mut self.output_capture_source_state
    }

    fn output_source_created(
        &mut self,
        source: smithay::wayland::image_capture_source::ImageCaptureSource,
        output: &smithay::output::Output,
    ) {
        // Weakly: a source outliving its monitor must not keep the output
        // alive, and a capture of a monitor that has been unplugged has
        // nothing to copy.
        source.user_data().insert_if_missing(|| output.downgrade());
        // Held, because the client destroys its own object as soon as it has
        // a session and the source is reference counted: letting it go here
        // stops the session out from under a capture that is already running.
        self.capture_sources.push(source);
    }
}

impl smithay::wayland::image_copy_capture::ImageCopyCaptureHandler for ViewportState {
    fn image_copy_capture_state(
        &mut self,
    ) -> &mut smithay::wayland::image_copy_capture::ImageCopyCaptureState {
        &mut self.image_copy_capture_state
    }

    fn capture_constraints(
        &mut self,
        source: &smithay::wayland::image_capture_source::ImageCaptureSource,
    ) -> Option<smithay::wayland::image_copy_capture::BufferConstraints> {
        let output = output_of(source)?;
        let size = output
            .current_mode()
            .map(|mode| output.current_transform().transform_size(mode.size))?;
        tracing::debug!(
            "capture constraints for {}: {}x{}, dmabuf {}",
            output.name(),
            size.w,
            size.h,
            if self.capture_dmabuf_constraints().is_some() { "yes" } else { "no" }
        );
        Some(smithay::wayland::image_copy_capture::BufferConstraints {
            size: (size.w, size.h).into(),
            // Shared memory only, as with screencopy: a client asking for a
            // picture has to be able to read the pixels, and XRGB rather than
            // ARGB because a screenshot has no transparency to carry.
            shm: vec![
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Xrgb8888,
            ],
            // And a dmabuf where there is a GPU to allocate on. A recorder
            // needs this one: shared memory means reading every pixel back
            // across the bus per frame, which is affordable for a screenshot
            // and not for a video.
            dma: self.capture_dmabuf_constraints(),
        })
    }

    fn new_session(&mut self, session: smithay::wayland::image_copy_capture::Session) {
        tracing::debug!("a capture session was created");
        // Held. Dropping a session sends `stopped` to the client, so letting
        // this one go is telling a recorder the compositor has stopped
        // capturing before it has begun — which it did, and the client's own
        // first `capture` came back failed.
        self.capture_sessions.push(session);
    }

    fn session_destroyed(
        &mut self,
        session: smithay::wayland::image_copy_capture::SessionRef,
    ) {
        self.capture_sessions.retain(|held| **held != session);
    }

    fn frame(
        &mut self,
        session: &smithay::wayland::image_copy_capture::SessionRef,
        frame: smithay::wayland::image_copy_capture::Frame,
    ) {
        tracing::debug!("a capture frame was asked for");
        let Some(output) = output_of(&session.source()) else {
            frame.fail(
                smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason::Stopped,
            );
            return;
        };
        self.pending_capture_frames.push((output, frame));
        // An idle desktop draws nothing, and a screenshot of an idle desktop
        // is the ordinary case.
        self.needs_render = true;
    }
}

/// The output a capture source names, if it still exists.
fn output_of(
    source: &smithay::wayland::image_capture_source::ImageCaptureSource,
) -> Option<smithay::output::Output> {
    source
        .user_data()
        .get::<smithay::output::WeakOutput>()
        .and_then(|weak| weak.upgrade())
}

impl crate::tearing::TearingControlHandler for ViewportState {
    fn tearing_control_state(&mut self) -> &mut crate::tearing::TearingControlState {
        &mut self.tearing_state
    }
}

impl crate::output_power::OutputPowerHandler for ViewportState {
    fn output_power_state(&mut self) -> &mut crate::output_power::OutputPowerState {
        &mut self.output_power_state
    }

    fn set_output_power(&mut self, output: &smithay::output::Output, on: bool) {
        ViewportState::set_output_power(self, output, on);
    }

    fn output_power(&mut self, output: &smithay::output::Output) -> bool {
        self.output_powered(output)
    }
}

impl crate::gamma::GammaControlHandler for ViewportState {
    fn gamma_control_state(&mut self) -> &mut crate::gamma::GammaControlState {
        &mut self.gamma_state
    }

    fn gamma_size(&mut self, output: &smithay::output::Output) -> Option<u32> {
        self.output_gamma_size(output)
    }

    fn set_gamma(
        &mut self,
        output: &smithay::output::Output,
        ramp: Option<&crate::gamma::Ramp>,
    ) -> bool {
        self.set_output_gamma(output, ramp)
    }
}

impl crate::output_management::OutputManagementHandler for ViewportState {
    fn output_management_state(
        &mut self,
    ) -> &mut crate::output_management::OutputManagementState {
        &mut self.output_management_state
    }

    fn current_heads(&mut self) -> Vec<crate::output_management::Head> {
        self.heads()
    }

    fn apply_output_configuration(
        &mut self,
        changes: &[crate::output_management::HeadChange],
        test_only: bool,
    ) -> bool {
        ViewportState::apply_output_configuration(self, changes, test_only)
    }
}

/// Middle-click paste: a second clipboard, separate from the ordinary one.
impl smithay::wayland::selection::primary_selection::PrimarySelectionHandler
    for ViewportState
{
    fn primary_selection_state(
        &mut self,
    ) -> &mut smithay::wayland::selection::primary_selection::PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

/// Clipboard managers, which need to watch selections they do not own — that
/// is the whole job of one, and the ordinary data-device protocol deliberately
/// does not allow it.
impl smithay::wayland::selection::wlr_data_control::DataControlHandler for ViewportState {
    fn data_control_state(
        &mut self,
    ) -> &mut smithay::wayland::selection::wlr_data_control::DataControlState {
        &mut self.data_control_state
    }
}

impl smithay::wayland::selection::ext_data_control::DataControlHandler for ViewportState {
    fn data_control_state(
        &mut self,
    ) -> &mut smithay::wayland::selection::ext_data_control::DataControlState {
        &mut self.ext_data_control_state
    }
}

/// Something asking the session not to go idle, which is what a video player
/// does while it is playing.
///
/// The surfaces are remembered rather than counted: a client that dies without
/// releasing its inhibitor would otherwise hold the screen awake for ever, and
/// a count cannot tell which one to forget.
impl smithay::wayland::idle_inhibit::IdleInhibitHandler for ViewportState {
    fn inhibit(&mut self, surface: WlSurface) {
        if !self.idle_inhibitors.contains(&surface) {
            self.idle_inhibitors.push(surface);
        }
        self.refresh_idle_inhibit();
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.idle_inhibitors.retain(|held| held != &surface);
        self.refresh_idle_inhibit();
    }
}

/// Clients that want to know the session went idle rather than asking the
/// compositor to do something about it — a chat program marking you away.
impl smithay::wayland::idle_notify::IdleNotifierHandler for ViewportState {
    fn idle_notifier_state(&mut self) -> &mut smithay::wayland::idle_notify::IdleNotifierState<Self> {
        &mut self.idle_notifier_state
    }
}

/// Fractional scaling. Nothing to decide: a client asking for it is told the
/// scale of the output it is on, and the render path already works in
/// fractional coordinates.
impl smithay::wayland::fractional_scale::FractionalScaleHandler for ViewportState {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        use smithay::wayland::compositor::with_states;
        use smithay::wayland::fractional_scale::with_fractional_scale;

        let scale = self
            .space
            .outputs()
            .next()
            .map(|output| output.current_scale().fractional_scale())
            .unwrap_or(1.0);
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}

/// The window list, for anything outside the compositor.
///
/// The shell already knows every window — it is drawing them — but nothing
/// outside does, and that is what a taskbar or an alt-tab replacement written
/// as an ordinary client needs.
impl smithay::wayland::foreign_toplevel_list::ForeignToplevelListHandler for ViewportState {
    fn foreign_toplevel_list_state(
        &mut self,
    ) -> &mut smithay::wayland::foreign_toplevel_list::ForeignToplevelListState {
        &mut self.foreign_toplevel_state
    }
}

/// Pointer capture: a game asking for the cursor to stop moving.
///
/// Activated the moment it is created if the pointer is already over the
/// surface, because that is when a game asks — mid-frame, with the cursor
/// where the click was.
impl smithay::wayland::pointer_constraints::PointerConstraintsHandler for ViewportState {
    fn new_constraint(
        &mut self,
        surface: &WlSurface,
        pointer: &smithay::input::pointer::PointerHandle<Self>,
    ) {
        use smithay::wayland::pointer_constraints::with_pointer_constraint;

        let over = self
            .surface_under(pointer.current_location())
            .map(|(under, _)| &under == surface)
            .unwrap_or(false);
        if !over {
            return;
        }
        with_pointer_constraint(surface, pointer, |constraint| {
            if let Some(constraint) = constraint {
                constraint.activate();
            }
        });
    }

    fn remove_constraint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
    ) {
        // Nothing to undo: the cursor was never moved while locked, so it is
        // already where the client left it.
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &smithay::input::pointer::PointerHandle<Self>,
        location: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
        // Where the cursor should reappear when the grab ends. A game usually
        // wants it back under the crosshair rather than wherever it happened
        // to be when the lock started (`src/pointer.c:104`).
        use smithay::wayland::seat::WaylandFocus as _;
        let Some(origin) = self
            .space
            .elements()
            .find(|window| {
                window
                    .wl_surface()
                    .map(|s| &*s == surface)
                    .unwrap_or(false)
            })
            .and_then(|window| self.space.element_geometry(window))
            .map(|geometry| geometry.loc)
        else {
            return;
        };
        let at = location + origin.to_f64();
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let under = self.surface_under(at);
        pointer.motion(
            self,
            under,
            &smithay::input::pointer::MotionEvent {
                location: at,
                serial,
                time: 0,
            },
        );
        self.needs_render = true;
    }
}

/// linux-dmabuf: how a client that renders on the GPU hands over its frames.
///
/// Without the global there is no way to present a GPU buffer at all. A Vulkan
/// client does not fall back to shared memory — its WSI has nothing to attach
/// to, and rio dies inside
/// vkGetPhysicalDeviceSurfaceCapabilitiesKHR with ERROR_SURFACE_LOST_KHR
/// rather than saying what is missing. Every hardware-accelerated client is in
/// the same position, so this is not one application's requirement.
impl smithay::wayland::dmabuf::DmabufHandler for ViewportState {
    fn dmabuf_state(&mut self) -> &mut smithay::wayland::dmabuf::DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &smithay::wayland::dmabuf::DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: smithay::wayland::dmabuf::ImportNotifier,
    ) {
        use smithay::backend::renderer::ImportDma as _;

        // Imported now rather than at first use, because the answer the client
        // is waiting for is whether this buffer is usable at all — and a
        // failure discovered mid-frame has nowhere to go.
        let imported = self
            .udev
            .as_mut()
            .map(|udev| udev.renderer.import_dmabuf(&dmabuf, None).is_ok());
        match imported {
            Some(true) | None => {
                let _ = notifier.successful::<ViewportState>();
            }
            Some(false) => notifier.failed(),
        }
    }
}

/// xdg-activation: "focus this, a token says it was asked for".
///
/// A launcher asks for a token before it starts a program, hands it over in the
/// environment, and the program presents it when its window appears — which is
/// how the window opens focused rather than behind whatever the user moved on
/// to. Without the global at all, wmenu aborts on an assertion before it draws.
///
/// The token is validated by smithay; all that is left is to honour it, and
/// only for a window the shell has been told about (`src/xdg_shell.c:546`).
impl smithay::wayland::xdg_activation::XdgActivationHandler for ViewportState {
    fn activation_state(&mut self) -> &mut smithay::wayland::xdg_activation::XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        _token: smithay::wayland::xdg_activation::XdgActivationToken,
        _data: smithay::wayland::xdg_activation::XdgActivationTokenData,
        surface: WlSurface,
    ) {
        let Some(view) = self.views.find_by_surface(&surface) else {
            return;
        };
        // A window that has not been announced has no rectangle to be focused
        // into, and the shell would be told to focus something it has never
        // heard of.
        if !view.mapped {
            return;
        }
        let id = view.id;
        crate::apply::focus_view(self, id);
    }
}

crate::delegate_screencopy!(ViewportState);
crate::delegate_output_management!(ViewportState);
crate::delegate_gamma_control!(ViewportState);
crate::delegate_output_power!(ViewportState);
crate::delegate_foreign_toplevel!(ViewportState);
crate::delegate_tearing_control!(ViewportState);

/// xdg-system-bell: a client asking the desktop to make a noise.
///
/// Logged rather than sounded. There is no audio path in this compositor and
/// no configuration for what a bell should be, and a terminal that rings one
/// on every tab completion is a client that would be very sorry to be taken
/// literally. Implementing the trait is what makes the global exist, which is
/// what stops a client treating its absence as an error.
impl smithay::wayland::xdg_system_bell::XdgSystemBellHandler for ViewportState {
    fn ring(&mut self, surface: Option<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>) {
        tracing::debug!("bell from {surface:?}");
    }
}

/// xdg-toplevel-tag: what a client calls its own windows.
///
/// A tag identifies one of an application's windows across restarts — its
/// terminal's scratchpad, its browser's picture-in-picture — which is exactly
/// what a session that restores a layout needs and cannot infer from a title.
/// Kept on the view so a window rule can match it.
impl smithay::wayland::xdg_toplevel_tag::XdgToplevelTagHandler for ViewportState {
    fn set_tag(
        &mut self,
        toplevel: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel,
        tag: String,
    ) {
        let Some(view) = self.view_for_toplevel(&toplevel) else {
            return;
        };
        tracing::debug!("view {view}: tagged {tag:?}");
        if let Some(view) = self.views.get_mut(view) {
            view.tag = Some(tag);
        }
    }

    fn set_description(
        &mut self,
        _toplevel: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel,
        _description: String,
    ) {
        // For a window switcher to show beside the title. Nothing here draws
        // one — the shell's taskbar has the title and the application — so it
        // is accepted and not kept.
    }
}

/// wp-pointer-warp: a client moving the pointer within its own surface.
///
/// Honoured only for the surface the pointer is already over, which is what
/// the protocol requires: a client that could warp the pointer onto itself
/// from anywhere could steal it from whatever the user was doing.
impl smithay::wayland::pointer_warp::PointerWarpHandler for ViewportState {
    fn warp_pointer(
        &mut self,
        surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        _pointer: smithay::reexports::wayland_server::protocol::wl_pointer::WlPointer,
        position: smithay::utils::Point<f64, smithay::utils::Logical>,
        serial: smithay::utils::Serial,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let at = pointer.current_location();
        let Some((under, origin)) = self.surface_under(at) else {
            return;
        };
        if under != surface {
            return;
        }

        // The client's position is relative to its own surface.
        let to = at - origin + position;
        let under = self.surface_under(to);
        pointer.motion(
            self,
            under,
            &smithay::input::pointer::MotionEvent {
                location: to,
                serial,
                time: self.start_time.elapsed().as_millis() as u32,
            },
        );
        pointer.frame(self);
        self.needs_render = true;
    }
}

smithay::delegate_dispatch2!(ViewportState);

/// A surface exported by one client and imported by another.
///
/// Pure bookkeeping: Smithay tracks the handles and resolves an imported
/// surface to the exported one, and the only thing a compositor has to supply
/// is where that lives. What it buys is a dialog opened on another client's
/// behalf — a portal's file chooser, most often — being parented to the window
/// that asked rather than floating loose in the middle of the desktop.
impl smithay::wayland::xdg_foreign::XdgForeignHandler for ViewportState {
    fn xdg_foreign_state(&mut self) -> &mut smithay::wayland::xdg_foreign::XdgForeignState {
        &mut self.xdg_foreign_state
    }
}

/// What a window says it looks like in a list.
///
/// The icon arrives as a name to look up in the theme, or as buffers the
/// client drew, and lands in the surface's cached state on commit. The shell
/// draws the taskbar and the overview, so it is told the name and looks it up
/// itself; the buffers are ignored, because handing the page a Wayland buffer
/// means a copy per icon per frame and the name is what a themed desktop
/// wants anyway.
impl smithay::wayland::xdg_toplevel_icon::XdgToplevelIconHandler for ViewportState {
    fn set_icon(
        &mut self,
        _toplevel: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel,
        wl_surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        // The icon is in the surface's *cached* state, which is where it lands
        // on commit — the protocol says the request is pending until then.
        let icon = smithay::wayland::compositor::with_states(&wl_surface, |states| {
            states
                .cached_state
                .get::<smithay::wayland::xdg_toplevel_icon::ToplevelIconCachedState>()
                .current()
                .icon_name()
                .map(|name| name.to_owned())
        });

        let Some(view) = self.views.find_by_surface_mut(&wl_surface) else {
            return;
        };
        if view.icon == icon {
            return;
        }
        view.icon = icon.clone();
        tracing::debug!("view {}: icon {icon:?}", view.id);
        // The same path a title change takes, so the icon reaches the outside
        // window list along with everything else that describes the window.
        self.notify_props(&wl_surface);
    }
}

/// An X11 client that wants every key.
///
/// Games and virtual machines ask for this so that the chords they use inside
/// themselves are not taken by the desktop around them. The grab itself is
/// Smithay's; what it needs from here is which focus target the X11 surface
/// belongs to, which is the surface itself — this compositor's keyboard focus
/// is a `WlSurface`, and an X11 window has one like any other client.
impl smithay::wayland::xwayland_keyboard_grab::XWaylandKeyboardGrabHandler for ViewportState {
    fn keyboard_focus_for_xsurface(
        &self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> Option<Self::KeyboardFocus> {
        Some(surface.clone())
    }
}

/// A sandbox asking for a socket of its own.
///
/// Flatpak and its like create one of these, hand the socket to the sandboxed
/// application, and every client that connects through it arrives tagged with
/// what the sandbox said about itself. The compositor does not act on the tag
/// yet — nothing here refuses a request on the strength of it — but the tag is
/// the part that cannot be added afterwards: a client that connected on the
/// ordinary socket is indistinguishable from any other for the rest of its
/// life.
///
/// The listener is a calloop source, so it goes into the same loop as the
/// compositor's own socket and produces streams the same way.
impl smithay::wayland::security_context::SecurityContextHandler for ViewportState {
    fn context_created(
        &mut self,
        source: smithay::wayland::security_context::SecurityContextListenerSource,
        context: smithay::wayland::security_context::SecurityContext,
    ) {
        tracing::info!(
            "a security context: engine {:?}, app {:?}",
            context.sandbox_engine,
            context.app_id
        );
        let inserted = self
            .loop_handle
            .insert_source(source, move |client_stream, _, state| {
                if let Err(e) = state.display_handle.insert_client(
                    client_stream,
                    std::sync::Arc::new(crate::state::ClientState {
                        security_context: Some(context.clone()),
                        ..Default::default()
                    }),
                ) {
                    tracing::warn!("a sandboxed client could not be let in: {e}");
                }
            });
        if let Err(e) = inserted {
            tracing::warn!("listening for a sandbox's clients: {e}");
        }
    }
}
