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
    // Not `WlSurface`: an X11 window has to be focused through the X server as
    // well, and smithay only does that for a focus that *is* an `X11Surface`.
    // See `keyboard_focus.rs`.
    type KeyboardFocus = crate::keyboard_focus::KeyboardFocus;
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
    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        self.cursor_status = image;
        // The pointer changing shape is a visible change with no other reason
        // to draw a frame behind it.
        self.needs_render = true;
    }

    fn focus_changed(
        &mut self,
        seat: &Seat<Self>,
        focused: Option<&crate::keyboard_focus::KeyboardFocus>,
    ) {
        let dh = &self.display_handle;
        let client = focused.and_then(|focus| dh.get_client(focus.surface().id()).ok());
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
            // The same thing a finger down, because a touchscreen drag is the
            // same gesture as a pointer one and a client that started it has
            // no way to tell which device it came from. The only difference is
            // that a touch grab takes no focus policy: there is no pointer to
            // leave behind, so the grab decides focus on its own.
            GrabType::Touch => {
                let Some(touch) = seat.get_touch() else {
                    source.cancel();
                    return;
                };
                let Some(start_data) = touch.grab_start_data() else {
                    source.cancel();
                    return;
                };
                let grab = DnDGrab::new_touch(&self.display_handle, start_data, source, seat);
                touch.set_grab(self, grab, serial);
            }
        }
    }
}

/// Tablet tools, which cursor-shape requires whether or not there is a tablet.
///
/// A tool can name its own cursor exactly as a pointer can, so the protocol's
/// dispatch asks for this. The focus type is the same surface every other input
/// uses, which is right — a pen points at a window like anything else.
///
/// The image is kept apart from the pointer's. They are two devices sharing one
/// visible cursor, and a drawing application setting a crosshair for the pen
/// has said nothing about what the mouse should be; folding them into one
/// status would let each overwrite the other's choice. `cursor_for` decides
/// which is showing, and the pen wins while it is in proximity because it is
/// the device being used.
impl smithay::input::tablet::TabletSeatHandler for ViewportState {
    type ToolFocus = WlSurface;

    fn tablet_tool_image(
        &mut self,
        _tool: &smithay::backend::input::TabletToolDescriptor,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        // Not filtered by which tool: there is one cursor on screen, and the
        // last tool to say something is the one being drawn with.
        self.tablet_cursor_status = Some(image);
        // Same reason the pointer's own callback does it — the picture
        // changing is a visible change with nothing else to prompt a frame.
        self.needs_render = true;
    }
}

impl OutputHandler for ViewportState {}

impl crate::screencopy::ScreencopyHandler for ViewportState {
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
    fn drm_syncobj_state(&mut self) -> Option<&mut smithay::wayland::drm_syncobj::DrmSyncobjState> {
        self.syncobj_state.as_mut()
    }
}

/// The standardised capture protocols, which is what a current
/// xdg-desktop-portal reaches for before wlr-screencopy.
///
/// A source is a thing that can be captured: a screen, or one window.
///
/// Windows were declined here for a while, on the grounds that a window
/// captured apart from the desktop loses the frame the shell drew around it
/// and so is not what the picker showed. The screencast portal then had to
/// offer windows anyway — offering them is the whole reason to own that
/// interface — and `read_window_pixels` composites a window's own surface tree
/// for it. So the capability exists and is already shipped; declining it only
/// here left a client that speaks the standard protocol worse off than one
/// going through the portal, for no difference in the picture either gets.
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

impl smithay::wayland::image_capture_source::ToplevelCaptureSourceHandler for ViewportState {
    fn toplevel_capture_source_state(
        &mut self,
    ) -> &mut smithay::wayland::image_capture_source::ToplevelCaptureSourceState {
        &mut self.toplevel_capture_source_state
    }

    fn toplevel_source_created(
        &mut self,
        source: smithay::wayland::image_capture_source::ImageCaptureSource,
        toplevel: smithay::wayland::foreign_toplevel_list::ForeignToplevelHandle,
    ) {
        // The view id, not the handle and not the window. A `Window` is a
        // handle into the `Space` that a remap invalidates; the id is what the
        // shell, the portal and the IPC all name this window by, and resolving
        // it at capture time is what makes a closed window fail rather than
        // draw whatever took its place.
        // By identifier: `ForeignToplevelHandle` is not `PartialEq` and its
        // `Arc` is private, but the identifier is the 32 random characters the
        // protocol itself uses to tell two toplevels apart.
        let wanted = toplevel.identifier();
        let Some(id) = self
            .views
            .iter()
            .find(|view| {
                view.foreign
                    .as_ref()
                    .is_some_and(|handle| handle.identifier() == wanted)
            })
            .map(|view| view.id)
        else {
            // A handle for a window that has already gone. The source stays
            // valid — the protocol has no way to refuse one — and every frame
            // asked of it fails, which is what a client that raced a close
            // has to handle anyway.
            tracing::debug!("a capture source was made from a toplevel that no longer exists");
            return;
        };
        source.user_data().insert_if_missing(|| ViewCapture(id));
        // Held for the same reason an output source is: the client destroys
        // its own object as soon as it has a session.
        self.capture_sources.push(source);
    }
}

/// The view a toplevel capture source names, in its user data.
///
/// A newtype rather than a bare `u32` so it cannot collide with anything else
/// stored there — `UserDataMap` is keyed by type.
struct ViewCapture(u32);

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
        let (what, size) = match target_of(self, source)? {
            crate::state::CaptureTarget::Output(output) => {
                let size = output
                    .current_mode()
                    .map(|mode| output.current_transform().transform_size(mode.size))?;
                (output.name(), size)
            }
            // The window's own size, upright, because that is what it is
            // composited at. Not the screen's, and not turned by the screen's
            // transform: a window is not rotated by the monitor it is on.
            crate::state::CaptureTarget::Window(id) => {
                let view = self.views.get(id)?;
                let geometry = self.space.element_geometry(&view.window)?;
                (
                    format!("view {id}"),
                    (geometry.size.w.max(1), geometry.size.h.max(1)).into(),
                )
            }
        };
        tracing::debug!(
            "capture constraints for {}: {}x{}, dmabuf {}",
            what,
            size.w,
            size.h,
            if self.capture_dmabuf_constraints().is_some() {
                "yes"
            } else {
                "no"
            }
        );
        Some(smithay::wayland::image_copy_capture::BufferConstraints {
            size: (size.w, size.h).into(),
            // Shared memory only, as with screencopy: a client asking for a
            // picture has to be able to read the pixels, and XRGB rather than
            // ARGB because a screenshot has no transparency to carry.
            shm: vec![smithay::reexports::wayland_server::protocol::wl_shm::Format::Xrgb8888],
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

    fn session_destroyed(&mut self, session: smithay::wayland::image_copy_capture::SessionRef) {
        self.capture_sessions.retain(|held| **held != session);
    }

    fn frame(
        &mut self,
        session: &smithay::wayland::image_copy_capture::SessionRef,
        frame: smithay::wayland::image_copy_capture::Frame,
    ) {
        tracing::debug!("a capture frame was asked for");
        // Stopped, not Unknown: the source named a screen that was unplugged
        // or a window that closed, and neither is coming back.
        let Some(target) = target_of(self, &session.source()) else {
            frame.fail(
                smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason::Stopped,
            );
            return;
        };
        self.pending_capture_frames.push((target, frame));
        // An idle desktop draws nothing, and a screenshot of an idle desktop
        // is the ordinary case.
        self.needs_render = true;
    }
}

/// What a capture source names, if it still exists.
///
/// Both arms can come back `None`, and for the same reason: a source outlives
/// what it points at. An unplugged monitor's `WeakOutput` stops upgrading and
/// a closed window's id stops resolving, which is how a session over either
/// one starts failing its frames instead of drawing something else.
fn target_of(
    state: &ViewportState,
    source: &smithay::wayland::image_capture_source::ImageCaptureSource,
) -> Option<crate::state::CaptureTarget> {
    if let Some(output) = source
        .user_data()
        .get::<smithay::output::WeakOutput>()
        .and_then(|weak| weak.upgrade())
    {
        return Some(crate::state::CaptureTarget::Output(output));
    }
    let id = source.user_data().get::<ViewCapture>()?.0;
    state
        .views
        .get(id)
        .map(|_| crate::state::CaptureTarget::Window(id))
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
    fn output_management_state(&mut self) -> &mut crate::output_management::OutputManagementState {
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
impl smithay::wayland::selection::primary_selection::PrimarySelectionHandler for ViewportState {
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
    fn idle_notifier_state(
        &mut self,
    ) -> &mut smithay::wayland::idle_notify::IdleNotifierState<Self> {
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
        // Unconditional, unlike the per-motion narration: a client asks for
        // capture a handful of times in a session, and which kind it asked
        // for and whether we activated it is the first thing anyone needs to
        // know when a game cannot look around.
        use smithay::wayland::pointer_constraints::PointerConstraint;
        let kind =
            with_pointer_constraint(surface, pointer, |constraint| match constraint.as_deref() {
                Some(PointerConstraint::Locked(_)) => "lock",
                Some(PointerConstraint::Confined(_)) => "confine",
                None => "gone",
            });
        tracing::info!(
            "pointer: a client asked for a {kind}, and the cursor is {} it",
            if over { "over" } else { "not over" }
        );
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
        surface: &WlSurface,
        pointer: &smithay::input::pointer::PointerHandle<Self>,
    ) {
        // The lock is over, so now the hint applies: put the cursor back
        // under the crosshair the client was drawing rather than wherever it
        // was pinned when the lock started (`src/pointer.c:104`).
        tracing::info!("pointer: a capture ended");
        self.apply_cursor_position_hint(surface, pointer);
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
        location: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
        // Recorded, not acted on. The protocol says the hint takes effect
        // when the lock is *deactivated*; moving the cursor now would also
        // send absolute motion to a client that asked for none.
        //
        // This is not a nicety. XWayland's warp emulator re-sends the hint
        // and commits the surface on every single relative motion event
        // while an X11 game holds the pointer, so acting on arrival turned
        // every mouse delta into an absolute reposition — which is exactly
        // what a game in GLFW's warp fallback reads as "the cursor did not
        // move", leaving the camera dead while clicks still worked.
        //
        // The first one unconditionally, because whether Xwayland sends these
        // at all is the question; the rest only when asked, because it sends
        // one per mouse delta.
        self.cursor_position_hints += 1;
        if self.cursor_position_hints == 1
            || (crate::pointer::debug() && self.cursor_position_hints % 100 == 1)
        {
            tracing::info!(
                "pointer: hint {} wants the cursor at {location:?} when the lock ends",
                self.cursor_position_hints
            );
        }
        self.cursor_position_hint = Some((surface.clone(), location));
    }
}

impl ViewportState {
    /// Move the cursor to a hint a lock left behind, if that surface left one.
    ///
    /// Surface-local, so it needs the window's position; a hint for a window
    /// that is gone is dropped rather than clamped to somewhere arbitrary.
    fn apply_cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &smithay::input::pointer::PointerHandle<Self>,
    ) {
        use smithay::wayland::seat::WaylandFocus as _;

        let Some((hinted, location)) = self.cursor_position_hint.take() else {
            return;
        };
        // A hint belongs to the surface that sent it. Another surface's lock
        // ending is not permission to use it, so put it back.
        if &hinted != surface {
            self.cursor_position_hint = Some((hinted, location));
            return;
        }
        let Some(origin) = self
            .space
            .elements()
            .find(|window| window.wl_surface().map(|s| &*s == surface).unwrap_or(false))
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
        // Without the frame the motion sits in the client's pending event
        // batch: wl_pointer only commits on frame, and XWayland is one of the
        // clients that waits for it.
        pointer.frame(self);
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
        let imported = self.udev.as_mut().map(|udev| {
            crate::with_gpu!(&mut udev.primary_mut().renderer, |renderer| renderer
                .import_dmabuf(&dmabuf, None)
                .is_ok())
        });
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
    fn ring(
        &mut self,
        surface: Option<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
    ) {
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
        Some(surface.clone().into())
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

/// The workspaces, which belong to the shell.
///
/// Nothing is decided here: a request from a client outside the shell is
/// forwarded as `workspace.request`, the shell does whatever it does with it,
/// and the next `workspace.list` says what happened. A shell that ignores the
/// message publishes a list nobody can change, which is a fair description of
/// a shell that has not implemented it.
impl crate::workspace::WorkspaceHandler for ViewportState {
    fn workspace_state(&mut self) -> &mut crate::workspace::WorkspaceState {
        &mut self.workspace_state
    }

    fn workspace_asked(&mut self, ask: crate::workspace::Ask) {
        use crate::workspace::Ask;
        let event = match ask {
            Ask::Activate(id) => viewport_ipc::event::Event::WorkspaceRequest {
                action: "activate".to_owned(),
                id: Some(id),
                name: None,
                output: None,
            },
            Ask::Deactivate(id) => viewport_ipc::event::Event::WorkspaceRequest {
                action: "deactivate".to_owned(),
                id: Some(id),
                name: None,
                output: None,
            },
            Ask::Remove(id) => viewport_ipc::event::Event::WorkspaceRequest {
                action: "remove".to_owned(),
                id: Some(id),
                name: None,
                output: None,
            },
            Ask::Assign { id, output } => viewport_ipc::event::Event::WorkspaceRequest {
                action: "assign".to_owned(),
                id: Some(id),
                name: None,
                output: Some(output),
            },
            Ask::Create { output, name } => viewport_ipc::event::Event::WorkspaceRequest {
                action: "create".to_owned(),
                id: None,
                name: Some(name),
                output: Some(output),
            },
        };
        tracing::debug!("workspace request out: {event:?}");
        self.notify(&event);
    }
}

impl crate::workspace::WorkspaceOutputs for ViewportState {
    fn workspace_outputs(&self) -> Vec<smithay::output::Output> {
        self.space.outputs().cloned().collect()
    }
}

/// Handing a connector to a client whole, with `wp-drm-lease-v1`.
///
/// The client is a VR runtime: it knows the headset's timing and its lens
/// distortion, and the compositor knows neither, so the honest thing is to
/// stop pretending it is a monitor and lease it the hardware. A lease is a
/// connector, a CRTC to drive it, and that CRTC's primary plane; dropping the
/// `DrmLease` revokes it, which is why they are held for as long as the client
/// keeps them.
///
/// Untested against a headset — there is not one here. The connectors offered
/// are those marked `non-desktop`, and on a machine with none of those this
/// advertises a global with nothing in it, which is the correct description of
/// a machine with nothing to lease.
impl smithay::wayland::drm_lease::DrmLeaseHandler for ViewportState {
    fn drm_lease_state(
        &mut self,
        _node: smithay::backend::drm::DrmNode,
    ) -> &mut smithay::wayland::drm_lease::DrmLeaseState {
        // One device, so one state — and this is only reached for a node that
        // has a global, which only the device that made one has.
        self.udev
            .as_mut()
            .and_then(|udev| udev.lease_state.as_mut())
            .expect("a lease request for a node with no lease state")
    }

    fn lease_request(
        &mut self,
        _node: smithay::backend::drm::DrmNode,
        request: smithay::wayland::drm_lease::DrmLeaseRequest,
    ) -> Result<
        smithay::wayland::drm_lease::DrmLeaseBuilder,
        smithay::wayland::drm_lease::LeaseRejected,
    > {
        use smithay::reexports::drm::control::Device as _;
        use smithay::wayland::drm_lease::{DrmLeaseBuilder, LeaseRejected};

        let Some(udev) = self.udev.as_mut() else {
            return Err(LeaseRejected::default());
        };
        let device = udev.primary().manager.device();
        let mut builder = DrmLeaseBuilder::new(device);

        // A CRTC that is not already driving one of this compositor's outputs,
        // and is legal for the connector asking. Handing over a CRTC that is
        // scanning out the desktop would take the desktop with it.
        let taken: std::collections::HashSet<_> =
            udev.ids().into_iter().map(|id| id.crtc).collect();
        let Ok(resources) = device.resource_handles() else {
            return Err(LeaseRejected::default());
        };

        for connector in request.connectors {
            let Ok(info) = device.get_connector(connector, false) else {
                return Err(LeaseRejected::default());
            };
            let crtc = info
                .encoders()
                .iter()
                .filter_map(|handle| device.get_encoder(*handle).ok())
                .flat_map(|encoder| resources.filter_crtcs(encoder.possible_crtcs()))
                .find(|crtc| !taken.contains(crtc));
            let Some(crtc) = crtc else {
                tracing::warn!("a lease was asked for with no free crtc to drive it");
                return Err(LeaseRejected::default());
            };
            let Ok(planes) = device.planes(&crtc) else {
                return Err(LeaseRejected::default());
            };
            // The claim is what stops the compositor's own allocator taking
            // the plane back while the client has it.
            let Some(claim) = device.claim_plane(planes.primary[0].handle, crtc) else {
                tracing::warn!("the primary plane for a leased crtc could not be claimed");
                return Err(LeaseRejected::default());
            };

            builder.add_connector(connector);
            builder.add_crtc(crtc);
            builder.add_plane(planes.primary[0].handle, claim);
        }

        Ok(builder)
    }

    fn new_active_lease(
        &mut self,
        _node: smithay::backend::drm::DrmNode,
        lease: smithay::wayland::drm_lease::DrmLease,
    ) {
        tracing::info!("a drm lease is active: {}", lease.id());
        if let Some(udev) = self.udev.as_mut() {
            udev.leases.push(lease);
        }
    }

    fn lease_destroyed(&mut self, _node: smithay::backend::drm::DrmNode, lease_id: u32) {
        tracing::info!("a drm lease ended: {lease_id}");
        if let Some(udev) = self.udev.as_mut() {
            udev.leases.retain(|lease| lease.id() != lease_id);
        }
    }
}
