// SPDX-License-Identifier: GPL-3.0-or-later
//
// Compositor state. Ports src/server.c.

use std::ffi::OsString;
use std::sync::Arc;

use smithay::backend::renderer::{Bind, ExportMem, Offscreen, Renderer};
use smithay::desktop::{PopupManager, Space, Window, WindowSurfaceType};
use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle, Resource as _};
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

/// Where an asset shipped with the compositor lives.
///
/// Beside the binary first — `<prefix>/bin/viewport` means
/// `<prefix>/share/viewport/…` — because an installed compositor is started
/// from wherever the user happened to be standing, and a path relative to the
/// working directory then finds nothing. The source tree is the fallback, for
/// a build run out of it.
///
/// Getting this wrong is a session with no shell at all: grey where the
/// wallpaper and the bar should be, and a load error naming a file in whatever
/// directory the login shell started in.
// Only the web engine's paths use this; dead, honestly, without it.
#[cfg_attr(not(feature = "wpe"), allow(dead_code))]
pub fn shipped_asset(relative: &str) -> String {
    if let Ok(exe) = std::env::current_exe() {
        // One more parent than looks right: a wrapped binary is
        // `<prefix>/bin/.viewport-wrapped`, and both forms want `<prefix>`.
        if let Some(prefix) = exe.parent().and_then(|bin| bin.parent()) {
            let installed = prefix.join("share/viewport").join(relative);
            if installed.exists() {
                return format!("file://{}", installed.display());
            }
        }
    }
    let here = std::env::current_dir().unwrap_or_default();
    format!("file://{}/data/{relative}", here.display())
}

/// A screenshot a client asked for and has not been given yet.
#[derive(Clone)]
pub struct PendingCopy {
    pub frame: smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    pub buffer: smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    pub output: Output,
    pub region: Rectangle<i32, smithay::utils::Physical>,
    pub overlay_cursor: bool,
    /// Whether the client used `copy_with_damage`, and so expects a damage
    /// event before `ready`.
    pub with_damage: bool,
}

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
    /// The D-Bus notification service, forwarding to the shell.
    pub notifications: crate::notification::Notifications,
    /// The settings portal, which is how a client learns the session is dark.
    pub appearance: crate::appearance::Appearance,
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
    /// How many popups the last frame drew, so a change is said once rather
    /// than per frame.
    pub popups_drawn: std::collections::HashMap<String, usize>,
    /// The binding mode in force — sway's resize mode, or anything a config
    /// file invents. Empty is the ordinary keymap.
    pub binding_mode: String,
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

    /// The headless backend's virtual outputs, when there is one.
    ///
    /// `Some` under `--headless` and nowhere else, which is what makes it the
    /// answer to "may this instance hotplug an output" — `output.test_add`
    /// exists so a test can plug a second monitor in without owning one.
    pub headless: Option<crate::headless::Headless>,

    /// Keys whose press was intercepted, so the matching release can be too.
    pub suppressed_keys: Vec<smithay::input::keyboard::Keysym>,

    /// Keybindings. Almost all of them are passthroughs to the shell.
    pub bindings: Vec<crate::binding::Binding>,

    /// Stops the outer GLib loop. calloop's own signal only ends the inner
    /// dispatch, so quitting has to go through this when the web engine is
    /// running.
    #[cfg(feature = "wpe")]

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
    /// Whether opening a Vulkan renderer for the shell's copy has already
    /// failed, so it is not attempted once per frame for the rest of the
    /// session.
    #[cfg(feature = "wpe")]
    pub shell_copy_refused: bool,
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
    /// How many times the shell has been restarted after its web process
    /// died, and when the run of restarts began.
    ///
    /// Both, because a restart limit on its own is wrong in each direction: a
    /// desktop up for a week that has crashed five times over that week is
    /// healthy, and one that crashes five times in five seconds is a page that
    /// cannot load and must not be retried forever. The window separates them.
    #[cfg(feature = "wpe")]
    pub shell_restarts: u32,
    #[cfg(feature = "wpe")]
    pub shell_restart_window: Option<std::time::Instant>,
    /// The shell element's identity, stable for the life of the compositor.
    ///
    /// A fresh `Id` per frame would make every damage tracker treat the shell
    /// as a new element each time, so it could never work out what actually
    /// changed and would repaint the whole output forever.
    #[cfg(feature = "wpe")]
    pub shell_element_id: smithay::backend::renderer::element::Id,
    /// A second id for the copy of the shell drawn *over* the windows.
    ///
    /// Its own, because the damage tracker keys on the id and one element
    /// appearing twice under a single id is a frame it cannot describe.
    /// One id per overlay rectangle, stable by position for as long as the
    /// list is that long: a render element whose id changes every frame tells
    /// the damage tracker everything is new.
    pub shell_overlay_ids: Vec<smithay::backend::renderer::element::Id>,
    /// Where the shell drew something that has to be above the windows, in the
    /// layout's own coordinates.
    ///
    /// Only the screen-share chooser so far. The shell is one buffer at the
    /// bottom of everything — the windows are painted into holes in it — so
    /// anything it draws is behind them by construction, which for a dialog
    /// asking a question is the one place that will not do.
    pub shell_overlays: Vec<smithay::utils::Rectangle<i32, Logical>>,
    /// What changed in the shell's buffer since the last frame.
    ///
    /// Required, not an optimisation. With a stable id the damage tracker
    /// decides whether to redraw by asking the element what changed, and an
    /// element built with `DamageSnapshot::empty()` answers "nothing" for
    /// ever — so the outputs go quiet after the first frame while WebKit
    /// carries on painting into buffers nobody draws.
    #[cfg(feature = "wpe")]
    pub shell_damage: smithay::backend::renderer::utils::DamageBag<i32, smithay::utils::Buffer>,

    /// The pointer image: the client's own surface where one is set, the
    /// theme's otherwise. Nothing draws a cursor unless this says what.
    pub cursor_status: smithay::input::pointer::CursorImageStatus,

    /// What a tablet tool asked its cursor to be, while it is in proximity.
    ///
    /// Separate from `cursor_status`, and not a replacement for it: the pen
    /// and the mouse are two devices sharing one visible cursor, and a
    /// drawing application setting a crosshair for the pen has said nothing
    /// about what the mouse should look like. Set on `set_cursor` from the
    /// tool, and cleared when the pen leaves proximity — a pen lifted away
    /// from the tablet is no longer the thing choosing the picture, and
    /// leaving its choice up would strand a crosshair under a mouse that has
    /// moved somewhere else entirely.
    pub tablet_cursor_status: Option<smithay::input::pointer::CursorImageStatus>,
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
    /// Outputs that need a frame, when it is known which ones do. `needs_render`
    /// means all of them, and stays the answer for anything that changes the
    /// desktop as a whole; this is for the cases that know better, such as a
    /// pacing barrier released for one screen's windows.
    pub dirty_outputs: std::collections::HashSet<crate::udev::OutputId>,

    /// wp_color_management_v1. Smithay has no handler for it, so the
    /// implementation is in crate::color_management.
    pub color_management: crate::color_management::ColorManagementState,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    /// xdg-dialog-v1: a client saying outright that a window is a dialog.
    ///
    /// Held rather than dropped, because dropping it takes the global down —
    /// and without the global the hint is never set, so every dialog is back to
    /// being inferred from whether it has a parent.
    pub _xdg_dialog_state: smithay::wayland::shell::xdg::dialog::XdgDialogState,
    /// xdg-system-bell: a client asking the desktop to make a noise.
    pub _system_bell_state: smithay::wayland::xdg_system_bell::XdgSystemBellState,
    /// xdg-toplevel-tag: what a client calls its own windows, so a session can
    /// tell two of them apart.
    pub _toplevel_tag_state: smithay::wayland::xdg_toplevel_tag::XdgToplevelTagManager,
    /// wp-pointer-warp: a client moving the pointer inside its own surface.
    // Kept alive rather than read: dropping the state withdraws the global.
    #[allow(dead_code)]
    pub pointer_warp_state: smithay::wayland::pointer_warp::PointerWarpManager,
    /// wlr-layer-shell: bars, launchers, notification daemons. Not the
    /// shell's business — a layer surface asks for an edge, not a layout.
    pub layer_shell_state: smithay::wayland::shell::wlr_layer::WlrLayerShellState,
    /// Middle-click paste. A separate clipboard from the ordinary one, and
    /// the one X11 applications and terminals expect.
    pub primary_selection_state:
        smithay::wayland::selection::primary_selection::PrimarySelectionState,
    /// Clipboard managers, which need to watch selections they do not own.
    pub data_control_state: smithay::wayland::selection::wlr_data_control::DataControlState,
    /// ext-data-control-v1: the same, standardised.
    pub ext_data_control_state: smithay::wayland::selection::ext_data_control::DataControlState,
    /// Something asking the session not to go idle — a video player.
    // Kept alive rather than read: dropping the state withdraws the global.
    #[allow(dead_code)]
    pub idle_inhibit_state: smithay::wayland::idle_inhibit::IdleInhibitManagerState,
    /// Clients that want to know when the session went idle, rather than
    /// asking the compositor to act on it.
    pub idle_notifier_state: smithay::wayland::idle_notify::IdleNotifierState<Self>,
    /// Surfaces that have been asked to hold idle off. Kept because a dead or
    /// hidden one must stop counting, and the client will not say so.
    pub idle_inhibitors: Vec<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,

    /// Buffer scaling and cropping, which a client uses to present a video at
    /// one size from a buffer of another without a copy.
    // Kept alive rather than read: dropping the state withdraws the global.
    #[allow(dead_code)]
    pub viewporter_state: smithay::wayland::viewporter::ViewporterState,
    /// When a frame actually reached the screen, which is what a video player
    /// synchronises audio against.
    // Kept alive rather than read: dropping the state withdraws the global.
    #[allow(dead_code)]
    pub presentation_state: smithay::wayland::presentation::PresentationState,
    /// A one-pixel buffer, so a client can fill a region with a colour without
    /// allocating one.
    // Kept alive rather than read: dropping the state withdraws the global.
    #[allow(dead_code)]
    pub single_pixel_state: smithay::wayland::single_pixel_buffer::SinglePixelBufferState,
    /// Fractional scaling: a client drawing at 1.25 rather than at 1 or 2.
    // Kept alive rather than read: dropping the state withdraws the global.
    #[allow(dead_code)]
    pub fractional_scale_state: smithay::wayland::fractional_scale::FractionalScaleManagerState,

    /// Pointer capture, and the relative motion a game reads instead of a
    /// position. Both are needed together: a lock with no relative motion
    /// leaves a game unable to turn at all.
    // Kept alive rather than read: dropping the state withdraws the global.
    #[allow(dead_code)]
    pub pointer_constraints_state: smithay::wayland::pointer_constraints::PointerConstraintsState,
    // Kept alive rather than read: dropping the state withdraws the global.
    #[allow(dead_code)]
    pub relative_pointer_state: smithay::wayland::relative_pointer::RelativePointerManagerState,
    /// ext-foreign-toplevel-list-v1: the window list, for anything outside the
    /// compositor. The shell already knows every window because it is drawing
    /// them; a taskbar or a switcher written as an ordinary client does not.
    pub foreign_toplevel_state: smithay::wayland::foreign_toplevel_list::ForeignToplevelListState,
    /// wlr-screencopy: screenshots and recording. Smithay implements it
    /// nowhere, so the dispatch is in `screencopy.rs`.
    // Kept alive rather than read: dropping the state withdraws the global.
    #[allow(dead_code)]
    pub screencopy_state: crate::screencopy::ScreencopyState,
    /// cursor-shape-v1: a client naming a cursor rather than drawing one.
    ///
    /// Kept only because the global has to outlive the display: nothing reads
    /// it, because a named shape arrives through `SeatHandler::cursor_image`
    /// like any other cursor.
    pub _cursor_shape_state: smithay::wayland::cursor_shape::CursorShapeManagerState,
    /// content-type-v1 and alpha-modifier-v1. Both are read where they are
    /// used — the content type by whatever decides about tearing, the alpha by
    /// Smithay's surface element — so neither is touched again after this.
    pub _content_type_state: smithay::wayland::content_type::ContentTypeState,
    pub _alpha_modifier_state: smithay::wayland::alpha_modifier::AlphaModifierState,
    /// tearing-control-v1: a client choosing latency over a whole frame.
    pub tearing_state: crate::tearing::TearingControlState,
    /// tablet-v2: drawing tablets, with pressure and tilt.
    pub _tablet_state: smithay::wayland::tablet_manager::TabletManagerState,
    /// pointer-gestures-v1: touchpad pinch, swipe and hold. Kept because the
    /// global has to outlive the display; the events go through the pointer.
    pub _pointer_gestures_state: smithay::wayland::pointer_gestures::PointerGesturesState,
    /// keyboard-shortcuts-inhibit-v1: a client asking for the chords the
    /// compositor would otherwise take.
    pub keyboard_shortcuts_inhibit_state:
        smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState,
    /// The inhibitors handed out, because Smithay's state offers no way to
    /// look one up by surface and the question asked on every key press is
    /// "does the surface with the keyboard have one".
    pub shortcut_inhibitors:
        Vec<smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitor>,
    /// text-input-v3, input-method-v2 and virtual-keyboard-v1: the three
    /// halves of an input method. Kept because the globals have to outlive the
    /// display; the conversation itself is Smithay's, and reaches the
    /// compositor only as a popup to place.
    pub _text_input_state: smithay::wayland::text_input::TextInputManagerState,
    pub _input_method_state: smithay::wayland::input_method::InputMethodManagerState,
    pub _virtual_keyboard_state: smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState,
    /// ext-image-capture-source-v1 and ext-image-copy-capture-v1: the
    /// standardised replacement for wlr-screencopy, and what a current
    /// xdg-desktop-portal reaches for first.
    // Kept alive rather than read: dropping the state withdraws the global.
    #[allow(dead_code)]
    pub image_capture_source_state: smithay::wayland::image_capture_source::ImageCaptureSourceState,
    pub output_capture_source_state:
        smithay::wayland::image_capture_source::OutputCaptureSourceState,
    pub toplevel_capture_source_state:
        smithay::wayland::image_capture_source::ToplevelCaptureSourceState,
    pub image_copy_capture_state: smithay::wayland::image_copy_capture::ImageCopyCaptureState,
    /// The capture sessions, held for the same reason as the sources: a
    /// dropped session sends `stopped` to its client, so letting one go is
    /// telling a recorder the compositor has stopped capturing.
    pub capture_sessions: Vec<smithay::wayland::image_copy_capture::Session>,
    /// The capture sources handed out, held so they outlive the client's own
    /// object. A client destroys the source as soon as it has a session — the
    /// protocol allows exactly that — and the source is reference counted, so
    /// dropping the compositor's copy stops the session the moment the client
    /// tidies up after itself.
    pub capture_sources: Vec<smithay::wayland::image_capture_source::ImageCaptureSource>,
    /// The GPU a client may allocate a capture buffer on, and the formats it
    /// may use — whichever backend is running fills this in, because they know
    /// different renderers and different nodes.
    pub capture_gpu: Option<(
        smithay::backend::drm::DrmNode,
        Vec<smithay::backend::allocator::Format>,
    )>,
    /// Capture frames waiting for the renderer, exactly as screencopy's are:
    /// the copy happens where the renderer is, which is inside a backend.
    pub pending_capture_frames: Vec<(CaptureTarget, smithay::wayland::image_copy_capture::Frame)>,
    /// linux-drm-syncobj-v1: a client saying when its buffer is ready rather
    /// than the kernel guessing. Absent on a GPU that cannot do it, and on
    /// the nested backend, which has no DRM device of its own.
    pub syncobj_state: Option<smithay::wayland::drm_syncobj::DrmSyncobjState>,
    /// wlr-foreign-toplevel-management: the window list a taskbar or a
    /// switcher can act on. The read-only ext protocol is beside it and
    /// describes the same windows.
    pub foreign_management_state: crate::foreign_toplevel::ForeignToplevelState,
    /// The screencast portal's streams, one per source a client is watching,
    /// and the PipeWire connection they live on. Absent until something asks
    /// to share a screen: a desktop nobody is sharing should not hold a
    /// connection open.
    pub pipewire: Option<crate::screencast::stream::Pipewire>,
    pub casts: Vec<crate::screencast::Cast>,
    /// A window being dragged with the pointer, and what the drag is doing to
    /// it.
    pub pointer_drag: Option<PointerDrag>,
    /// Whether the pointer is over the shell rather than over a client.
    ///
    /// Kept because the transitions are what matter: the shell has to be told
    /// when the pointer leaves it, or a `:hover` stays lit under whatever the
    /// pointer moved on to.
    pub pointer_on_shell: bool,
    /// A button pressed on the shell holds the pointer until it is released.
    ///
    /// Without it, dragging the divider between two windows breaks the moment
    /// the cursor crosses onto a window: hit-testing would start routing motion
    /// to that client and the shell would never see the rest of the drag.
    /// Wayland gives clients an implicit grab for exactly this reason
    /// (`src/input.c:237`).
    pub pointer_grabbed_by_shell: bool,
    /// The chooser that is up, while an application is waiting to be told what
    /// it may share.
    pub picker: Option<crate::screencast::Picker>,
    /// Which request the next chooser is for. Rising rather than reused so a
    /// stale answer — a timer that fired while the user was still deciding —
    /// cannot be applied to the chooser that replaced it.
    next_pick: u32,
    /// wlr-output-power-management: what wlopm and a lid-close script speak.
    pub output_power_state: crate::output_power::OutputPowerState,
    /// wlr-gamma-control: what wlsunset and gammastep speak. Smithay
    /// implements it nowhere, so the dispatch is in `gamma.rs`.
    pub gamma_state: crate::gamma::GammaControlState,
    /// The ramp each output is wearing, so a VT switch back can put it on
    /// again: the kernel resets gamma when the session is handed over, and a
    /// night-light client has no way to know it happened.
    pub gamma_ramps: std::collections::HashMap<String, crate::gamma::Ramp>,
    /// wlr-output-management: what kanshi, wlr-randr and wdisplays speak.
    /// Smithay implements it nowhere, so the dispatch is in
    /// `output_management.rs`.
    pub output_management_state: crate::output_management::OutputManagementState,
    /// Copies asked for and not yet made. Served the next time the output
    /// they name is drawn, because that is where its renderer is.
    pub pending_copies: Vec<PendingCopy>,
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
    /// A surface one client can hand to another by name, so a dialog opened on
    /// another client's behalf is parented to the window that asked for it
    /// rather than floating loose.
    pub xdg_foreign_state: smithay::wayland::xdg_foreign::XdgForeignState,
    /// wp-fifo: a client asking for its frames to be paced by the display
    /// rather than drawn as fast as it can manage.
    ///
    /// A client that waits on a barrier is *blocked* until the compositor
    /// signals it, so advertising this and never signalling is a client that
    /// paints once and stops. It is signalled in `release_frame_barriers`, and
    /// — the part that was missing the first time — `tick_barriers` keeps a
    /// clock running while any barrier is outstanding, because a blocked
    /// commit produces no damage and this compositor draws on damage.
    // Kept alive rather than read: dropping the state withdraws the global.
    #[allow(dead_code)]
    pub fifo_state: Option<smithay::wayland::fifo::FifoManagerState>,
    /// wp-commit-timing: a client asking for a commit to take effect at a
    /// particular time rather than at once. Blocked the same way, released in
    /// the same place, and kept alive by the same tick.
    // Kept alive rather than read: dropping the state withdraws the global.
    #[allow(dead_code)]
    pub commit_timing_state: Option<smithay::wayland::commit_timing::CommitTimingManagerState>,
    /// Whether a barrier tick is already armed, so a hundred commits do not
    /// arm a hundred timers.
    pub barrier_tick: bool,
    /// A frame-callback tick is already armed.
    pub frame_clock: bool,
    /// When that tick is due. Kept for the log and for anything that wants to
    /// know how far off the next invitation is.
    pub frame_clock_at: Option<std::time::Instant>,
    /// Someone asked for a tick since the last one ran.
    ///
    /// The tick after a commit is not always the one that answers it: frame
    /// callbacks are throttled to one a refresh per surface, so a commit
    /// landing just after a callback went out finds the next tick refusing to
    /// send another. Something has to bring the tick after *that* one around,
    /// and nothing else will — a client waiting on an invitation makes no
    /// damage, and no damage is what stops the clock. So a request is
    /// remembered across one tick.
    pub frame_pending: bool,
    /// The timer the frame clock is armed on, once it has been created.
    ///
    /// A timerfd rather than one of calloop's own timers. calloop keeps those
    /// in a wheel it consults while it is doing the waiting, and under the web
    /// engine it is not doing the waiting: GLib owns the blocking poll and
    /// watches calloop's epoll fd as a single source, so a wheel entry is
    /// invisible to it. An expiring timerfd makes that epoll fd readable like
    /// any other event would, which is the whole difference between a tick
    /// that arrives on a still desktop and one that waits for the mouse to
    /// move.
    pub frame_timer: Option<std::os::fd::OwnedFd>,
    /// The timer the barrier tick is armed on. Same reasoning, and the same
    /// consequence for getting it wrong: see [`ViewportState::arm_barrier_tick`].
    pub barrier_timer: Option<std::os::fd::OwnedFd>,
    /// How many ticks in a row have released nothing.
    pub barrier_quiet: u32,
    /// The shell's workspaces, mirrored for `ext-workspace-v1`. Empty until
    /// the shell says otherwise, which is the truthful description of a
    /// desktop whose workspaces nobody has published.
    pub workspace_state: crate::workspace::WorkspaceState,
    /// Held because dropping it takes the global with it.
    pub _security_context_state: smithay::wayland::security_context::SecurityContextState,
    /// What a window says it looks like in a list. Held because dropping it
    /// takes the global with it.
    pub _xdg_toplevel_icon_manager: smithay::wayland::xdg_toplevel_icon::XdgToplevelIconManager,
    /// An X11 client asking for every key — a game, or a virtual machine.
    pub _xwayland_keyboard_grab_state:
        smithay::wayland::xwayland_keyboard_grab::XWaylandKeyboardGrabState,
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
    // Kept alive rather than read: dropping the state withdraws the global.
    #[allow(dead_code)]
    pub xdg_decoration_state: smithay::wayland::shell::xdg::decoration::XdgDecorationState,
    pub shm_state: ShmState,
    // Kept alive rather than read: dropping the state withdraws the global.
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub seat: Seat<Self>,
}

/// What a capture is a picture of.
///
/// A view id rather than a window for the second one: a `Window` is a handle
/// into the `Space` that a remap invalidates, while the id is what the shell,
/// the screencast portal and the IPC all name the same thing by. Resolving it
/// late means a capture of a window that has closed fails rather than draws
/// something else.
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureTarget {
    Output(Output),
    Window(u32),
}

/// The surfaces one window is drawn from, and the size they cover.
///
/// Generic over the renderer because `with_gpu!` compiles every render body
/// twice, once per backend.
type WindowElements<R> = (
    Vec<smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<R>>,
    smithay::utils::Size<i32, smithay::utils::Physical>,
);

impl ViewportState {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        socket_path: Option<std::path::PathBuf>,
    ) -> anyhow::Result<Self> {
        let dh = display.handle();
        let loop_handle = event_loop.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let color_management = crate::color_management::ColorManagementState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let _xdg_dialog_state =
            smithay::wayland::shell::xdg::dialog::XdgDialogState::new::<Self>(&dh);
        let _system_bell_state =
            smithay::wayland::xdg_system_bell::XdgSystemBellState::new::<Self>(&dh);
        let _toplevel_tag_state =
            smithay::wayland::xdg_toplevel_tag::XdgToplevelTagManager::new::<Self>(&dh);
        let pointer_warp_state =
            smithay::wayland::pointer_warp::PointerWarpManager::new::<Self>(&dh);
        let layer_shell_state =
            smithay::wayland::shell::wlr_layer::WlrLayerShellState::new::<Self>(&dh);
        let screencopy_state = crate::screencopy::ScreencopyState::new::<Self>(&dh);
        let output_management_state =
            crate::output_management::OutputManagementState::new::<Self>(&dh);
        let gamma_state = crate::gamma::GammaControlState::new::<Self>(&dh);
        let output_power_state = crate::output_power::OutputPowerState::new::<Self>(&dh);
        let foreign_management_state =
            crate::foreign_toplevel::ForeignToplevelState::new::<Self>(&dh);
        // The standardised capture protocols. wlr-screencopy stays beside
        // them: grim and wf-recorder speak that one and nothing else, while a
        // current xdg-desktop-portal looks for these first.
        let image_capture_source_state =
            smithay::wayland::image_capture_source::ImageCaptureSourceState::new();
        let output_capture_source_state =
            smithay::wayland::image_capture_source::OutputCaptureSourceState::new::<Self>(&dh);
        // Windows as well as screens. The picker in a browser's "share your
        // screen" dialogue lists both, and a client that binds this manager
        // and finds nothing behind it has no way to offer the second.
        let toplevel_capture_source_state =
            smithay::wayland::image_capture_source::ToplevelCaptureSourceState::new::<Self>(&dh);
        let image_copy_capture_state =
            smithay::wayland::image_copy_capture::ImageCopyCaptureState::new::<Self>(&dh);
        // Input methods. Three protocols that only work together: the
        // application says where its text is going through text-input, the
        // input method reads that and sends back what was composed, and
        // virtual-keyboard is how an on-screen keyboard turns a tap into a key.
        //
        // Any client may be an input method here. Restricting it needs a
        // notion of a privileged client, which this compositor does not have —
        // and a filter that everything passes is worse than none, because it
        // reads as though it were deciding something.
        // Tearing, for a full-screen game that would rather have the newest
        // frame part-drawn than the previous one whole.
        let tearing_state = crate::tearing::TearingControlState::new::<Self>(&dh);
        // Drawing tablets. The manager is the global; the tablets themselves
        // are added to the seat as libinput reports them, because a client is
        // told about each device and there is no honest way to describe one
        // that is not plugged in.
        let tablet_state = smithay::wayland::tablet_manager::TabletManagerState::new::<Self>(&dh);
        // Touchpad gestures. A client that cannot see them has no way to tell
        // a two-finger scroll from a three-finger swipe, because everything
        // else it is sent is scroll.
        let pointer_gestures_state =
            smithay::wayland::pointer_gestures::PointerGesturesState::new::<Self>(&dh);
        // A client asking for the chords the compositor would otherwise take.
        // A virtual machine and a remote desktop both need Mod4 to reach the
        // session inside them rather than the one around it.
        let keyboard_shortcuts_inhibit_state =
            smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState::new::<Self>(
                &dh,
            );
        let text_input_state =
            smithay::wayland::text_input::TextInputManagerState::new::<Self>(&dh);
        let input_method_state = smithay::wayland::input_method::InputMethodManagerState::new::<
            Self,
            _,
        >(&dh, |_client| true);
        let virtual_keyboard_state =
            smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState::new::<Self, _>(
                &dh,
                |_client| true,
            );
        // A client that names its cursor rather than drawing one. Without it a
        // GTK application shows the pointer it inherited from whatever it last
        // hovered, because it has no other way to ask for a text caret.
        let cursor_shape_state =
            smithay::wayland::cursor_shape::CursorShapeManagerState::new::<Self>(&dh);
        // What a surface is showing — video, a game — which is what a
        // compositor would decide tearing and refresh from.
        let content_type_state = smithay::wayland::content_type::ContentTypeState::new::<Self>(&dh);
        // A multiplier a client applies to its own surface. Smithay's surface
        // element reads it while building the render element, so honouring it
        // is the global and nothing else.
        let alpha_modifier_state =
            smithay::wayland::alpha_modifier::AlphaModifierState::new::<Self>(&dh);
        let primary_selection_state =
            smithay::wayland::selection::primary_selection::PrimarySelectionState::new::<Self>(&dh);
        let data_control_state =
            smithay::wayland::selection::wlr_data_control::DataControlState::new::<Self, _>(
                &dh,
                Some(&primary_selection_state),
                |_| true,
            );
        // The newer clipboard-manager protocol beside the wlroots one. Both
        // do the same job and clients are moving between them: cliphist and
        // wl-clipboard bind whichever they find, and a session that publishes
        // only the old one loses the newer builds.
        let ext_data_control_state =
            smithay::wayland::selection::ext_data_control::DataControlState::new::<Self, _>(
                &dh,
                Some(&primary_selection_state),
                |_| true,
            );
        let idle_inhibit_state =
            smithay::wayland::idle_inhibit::IdleInhibitManagerState::new::<Self>(&dh);
        let idle_notifier_state =
            smithay::wayland::idle_notify::IdleNotifierState::<Self>::new(&dh, loop_handle.clone());
        let viewporter_state = smithay::wayland::viewporter::ViewporterState::new::<Self>(&dh);
        // CLOCK_MONOTONIC: the same clock every timestamp in this compositor
        // uses, and the one a client compares against.
        let presentation_state = smithay::wayland::presentation::PresentationState::new::<Self>(
            &dh,
            smithay::reexports::rustix::time::ClockId::Monotonic as u32,
        );
        let single_pixel_state =
            smithay::wayland::single_pixel_buffer::SinglePixelBufferState::new::<Self>(&dh);
        let fractional_scale_state =
            smithay::wayland::fractional_scale::FractionalScaleManagerState::new::<Self>(&dh);
        let foreign_toplevel_state =
            smithay::wayland::foreign_toplevel_list::ForeignToplevelListState::new::<Self>(&dh);
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
        let xdg_foreign_state = smithay::wayland::xdg_foreign::XdgForeignState::new::<Self>(&dh);
        // On, and VIEWPORT_FIFO=0 turns them off.
        //
        // These froze clients twice — taken out once (`c7c4433`), brought back
        // (`7a48b16`), and still freezing on 2026-07-30: one terminal on an
        // otherwise empty workspace stopped showing what was typed into it
        // until a second window appeared. Not a corner of the desktop, because
        // Mesa paces FIFO present mode with `wp_fifo_v1` — so it was every
        // client painting through Vulkan or EGL, which is most of them.
        //
        // Both causes are fixed rather than avoided: presentation feedback was
        // never sent to anybody because nothing recorded which screen a surface
        // was on, and fifo barriers were signalled where they lay instead of
        // being taken, in both halves of the double buffer. See
        // `released_frame_barriers` and `update_scanout_outputs`.
        //
        // The switch stays because it is what found this. One run with it off
        // separated "the pacing protocols" from "everything else" in a minute,
        // after a day of reading code that did not.
        let pacing = std::env::var("VIEWPORT_FIFO").as_deref() != Ok("0");
        // Separately, because the two are different protocols and a client
        // uses both: Mesa sets a fifo barrier *and* a commit-timing deadline
        // on the same frame. Turning them off together says the pacing
        // protocols are involved and stops there, which is where the last
        // bisect ran out of resolution.
        let timing = pacing && std::env::var("VIEWPORT_COMMIT_TIMING").as_deref() != Ok("0");
        let fifo_state = pacing.then(|| smithay::wayland::fifo::FifoManagerState::new::<Self>(&dh));
        let commit_timing_state = timing
            .then(|| smithay::wayland::commit_timing::CommitTimingManagerState::new::<Self>(&dh));
        let workspace_state = crate::workspace::WorkspaceState::new::<Self>(&dh);
        let security_context_state =
            smithay::wayland::security_context::SecurityContextState::new::<Self, _>(
                &dh,
                // A sandboxed client must not be able to hand out sandboxes of
                // its own: that is how a restriction gets laundered into none.
                |client| {
                    client
                        .get_data::<ClientState>()
                        .map(|data| data.security_context.is_none())
                        .unwrap_or(true)
                },
            );
        let xdg_toplevel_icon_manager =
            smithay::wayland::xdg_toplevel_icon::XdgToplevelIconManager::new::<Self>(&dh);
        let xwayland_keyboard_grab_state =
            smithay::wayland::xwayland_keyboard_grab::XWaylandKeyboardGrabState::new::<Self>(&dh);
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
        // A touchscreen. Added unconditionally, as the pointer and keyboard
        // are: the seat's capabilities are what a client checks before it
        // listens for anything, and a device appearing later cannot make a
        // client that has already decided start listening.
        seat.add_touch();

        let socket_name = Self::init_wayland_listener(display, event_loop)?;

        // The control socket is named after the Wayland display, so it has to
        // wait until the display exists.
        let path = socket_path.unwrap_or_else(|| Ipc::default_path(socket_name.to_str()));
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
                // Off the end of a monitor carries on to the next one, which
                // is what this has always done and what sway does.
                focus_crosses_outputs: true,
                // The tree of splits the shell has always built; a dynamic
                // mode is opt-in.
                tiling_mode: None,
            },
            shell_url: None,
            output_config: std::collections::HashMap::new(),
            startup: None,
            notifications: crate::notification::Notifications::default(),
            appearance: crate::appearance::Appearance::default(),
            status: crate::status::Status::default(),
            idle: crate::idle::Idle::default(),
            idle_settings: crate::idle::Settings::default(),
            vt_switching: true,
            server_decorations: true,
            dark_mode: true,
            popups_drawn: std::collections::HashMap::new(),
            binding_mode: String::new(),
            adaptive_sync: false,
            fallback_url: None,
            // C's default (`src/main.c:54`). The deadline is on the first
            // painted frame, not on the load event.
            load_timeout_ms: 5000,
            logo_held: false,
            overview: false,
            active_output: None,
            udev: None,
            headless: None,
            suppressed_keys: Vec::new(),
            bindings: crate::binding::defaults(
                &std::env::var("VIEWPORT_TERMINAL").unwrap_or_else(|_| "foot".to_owned()),
                &std::env::var("VIEWPORT_MENU").unwrap_or_else(|_| "wmenu-run".to_owned()),
                // The starting keymap, before a config file has been read. The
                // layout it is built for is the one `self.config.layout` also
                // starts at; reload_bindings() rebuilds it against whatever the
                // file turned out to say.
                "tiling",
            ),
            #[cfg(feature = "wpe")]
            shell: None,
            #[cfg(feature = "wpe")]
            shell_ping: None,
            #[cfg(feature = "wpe")]
            shell_size: None,
            #[cfg(feature = "wpe")]
            shell_renderer: None,
            #[cfg(feature = "wpe")]
            shell_copy_refused: false,
            #[cfg(feature = "wpe")]
            shell_owned: None,
            #[cfg(feature = "wpe")]
            shell_frames: 0,
            #[cfg(feature = "wpe")]
            shell_restarts: 0,
            #[cfg(feature = "wpe")]
            shell_restart_window: None,
            #[cfg(feature = "wpe")]
            shell_element_id: smithay::backend::renderer::element::Id::new(),
            shell_overlay_ids: Vec::new(),
            shell_overlays: Vec::new(),
            #[cfg(feature = "wpe")]
            shell_damage: Default::default(),

            cursor_status: smithay::input::pointer::CursorImageStatus::default_named(),
            tablet_cursor_status: None,
            cursor_theme: crate::cursor::Theme::new(),
            cursor_warned: false,
            last_layout: None,
            needs_render: false,
            dirty_outputs: std::collections::HashSet::new(),

            color_management,
            compositor_state,
            xdg_shell_state,
            _xdg_dialog_state,
            _system_bell_state,
            _toplevel_tag_state,
            pointer_warp_state,
            layer_shell_state,
            screencopy_state,
            tearing_state,
            _tablet_state: tablet_state,
            _pointer_gestures_state: pointer_gestures_state,
            keyboard_shortcuts_inhibit_state,
            shortcut_inhibitors: Vec::new(),
            _text_input_state: text_input_state,
            _input_method_state: input_method_state,
            _virtual_keyboard_state: virtual_keyboard_state,
            gamma_state,
            output_power_state,
            pipewire: None,
            casts: Vec::new(),
            pointer_drag: None,
            pointer_on_shell: false,
            pointer_grabbed_by_shell: false,
            picker: None,
            next_pick: 1,
            foreign_management_state,
            image_capture_source_state,
            output_capture_source_state,
            toplevel_capture_source_state,
            image_copy_capture_state,
            capture_sessions: Vec::new(),
            capture_sources: Vec::new(),
            capture_gpu: None,
            pending_capture_frames: Vec::new(),
            syncobj_state: None,
            gamma_ramps: std::collections::HashMap::new(),
            _cursor_shape_state: cursor_shape_state,
            _content_type_state: content_type_state,
            _alpha_modifier_state: alpha_modifier_state,
            output_management_state,
            pending_copies: Vec::new(),
            primary_selection_state,
            data_control_state,
            ext_data_control_state,
            idle_inhibit_state,
            idle_notifier_state,
            idle_inhibitors: Vec::new(),
            viewporter_state,
            presentation_state,
            single_pixel_state,
            fractional_scale_state,
            foreign_toplevel_state,
            pointer_constraints_state,
            relative_pointer_state,
            session_lock_state,
            locked: false,
            locked_at: None,
            lock_warned: false,
            lock_surfaces: std::collections::HashMap::new(),
            xdg_activation_state,
            xdg_foreign_state,
            workspace_state,
            fifo_state,
            commit_timing_state,
            barrier_tick: false,
            frame_clock: false,
            frame_clock_at: None,
            frame_pending: false,
            frame_timer: None,
            barrier_timer: None,
            barrier_quiet: 0,
            _security_context_state: security_context_state,
            _xdg_toplevel_icon_manager: xdg_toplevel_icon_manager,
            _xwayland_keyboard_grab_state: xwayland_keyboard_grab_state,
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

    /// Bind the Wayland socket and start listening on it.
    ///
    /// Every failure here is fatal — a compositor with no socket is a
    /// compositor no client can reach — but fatal is not the same as a panic.
    /// The socket is the first thing that touches the outside world, so it is
    /// where a session that is not set up properly shows itself, and the
    /// commonest of those is worth naming rather than unwrapping.
    fn init_wayland_listener(
        display: Display<Self>,
        event_loop: &mut EventLoop<'static, Self>,
    ) -> anyhow::Result<OsString> {
        let listening_socket = ListeningSocketSource::new_auto().map_err(|e| {
            // The socket lives in XDG_RUNTIME_DIR, so without one there is
            // nowhere to put it. A login session sets this; a compositor
            // started by hand, from a bare `su`, or inside a container often
            // has no session behind it and inherits nothing.
            if std::env::var_os("XDG_RUNTIME_DIR").is_none_or(|dir| dir.is_empty()) {
                anyhow::anyhow!(
                    "XDG_RUNTIME_DIR is not set, so there is nowhere to put the \
                     Wayland socket.\n\
                     It is normally set for you by the login session. If you are \
                     starting the compositor by hand — from a bare `su`, a cron \
                     job, or a container — point it at a private writable \
                     directory you own, conventionally /run/user/$(id -u):\n\
                     \n    export XDG_RUNTIME_DIR=/run/user/$(id -u)"
                )
            } else {
                anyhow::Error::from(e).context(
                    "could not bind a Wayland socket. Every name from wayland-1 to \
                     wayland-32 is taken, or XDG_RUNTIME_DIR is not writable",
                )
            }
        })?;

        let socket_name = listening_socket.socket_name().to_os_string();
        let loop_handle = event_loop.handle();

        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                if let Err(e) = state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                {
                    // One client that could not be taken on. It is the only
                    // thing affected, and the desktop around it carries on —
                    // which is not true if this unwinds through the event loop.
                    tracing::error!("could not accept a client connection: {e}");
                }
            })
            .map_err(|e| anyhow::anyhow!("listening for Wayland clients: {e}"))?;

        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // Timed, when the counters are on: this is every request
                    // every client sends, parsed and handed to a handler, and
                    // it is the one part of a loop turn that is ours to
                    // measure. What is left after it is calloop waking up.
                    let started = state
                        .udev
                        .as_ref()
                        .and_then(|udev| udev.frame_log.as_ref())
                        .map(|_| std::time::Instant::now());

                    // Safety: the display is not dropped here.
                    let messages = unsafe { display.get_mut().dispatch_clients(state).unwrap() };

                    if let Some(started) = started {
                        let spent = started.elapsed().as_nanos() as u64;
                        if let Some(log) =
                            state.udev.as_mut().and_then(|udev| udev.frame_log.as_mut())
                        {
                            state_dispatches(log, spent, messages as u64);
                        }
                    }
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| anyhow::anyhow!("dispatching Wayland clients: {e}"))?;

        Ok(socket_name)
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
        if crate::pointer::over_overlay(&self.shell_overlays, pos) {
            // The shell drew something here in front of the windows — a
            // notification, a floating bar, the screen-share chooser. It is on
            // top, so it takes the pointer; reporting the window underneath
            // would hand the click straight through it.
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
        // Every window, topmost first, asked directly rather than through
        // `Space::element_under`.
        //
        // That helper finds a window whose own rectangle contains the point,
        // and a menu overflows the window that opened it — so a click on the
        // part of a Firefox menu hanging past the window edge found nothing
        // and went to whatever was behind, which is a menu that cannot be
        // used. `Window::surface_under` looks through the popups as well.
        let mut windows: Vec<(smithay::desktop::Window, Point<i32, Logical>)> = self
            .space
            .elements()
            .filter_map(|window| {
                self.space
                    .element_location(window)
                    .map(|location| (window.clone(), location))
            })
            .collect();
        windows.reverse();

        for (window, location) in windows {
            // Where the surface is drawn, not where the window is mapped.
            //
            // A client with client-side decorations draws its shadows outside
            // the window: xdg_surface.geometry marks the real window inside a
            // larger surface, and its origin is frequently negative. The map
            // location is the window's, so surface-local coordinates have to
            // start from the surface's — which is what `Space::element_under`
            // returns and what reading the map location instead got wrong, by
            // exactly the width of the shadow.
            let render_location = location - window.geometry().loc;
            if let Some((surface, at)) =
                window.surface_under(pos - render_location.to_f64(), WindowSurfaceType::ALL)
            {
                return Some((surface, (at + render_location).to_f64()));
            }
        }
        below
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

    /// Copy every frame waiting on `output`, and answer its client.
    ///
    /// Generic over the renderer because the two backends have different ones
    /// and neither is reachable from where the request arrives: the nested
    /// backend's lives inside its event loop. A backend calls this while it
    /// holds its renderer, right after it has drawn.
    ///
    /// Composited fresh rather than read back from the scanout buffer: the
    /// front buffer holds whatever was last flipped, which for an idle screen
    /// is a frame of unknown age — for a screenshot that is the difference
    /// between the current desktop and one from a minute ago.
    pub fn service_screencopy<R, B>(&mut self, output: &Output, renderer: &mut R)
    where
        R: Renderer
            + Bind<B>
            + Offscreen<B>
            + ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        if self.pending_copies.is_empty() {
            return;
        }
        // Only this output's. A second monitor's copies wait for that monitor
        // to draw, which is where its renderer will be.
        let mut mine = Vec::new();
        self.pending_copies.retain(|copy| {
            if copy.output == *output {
                mine.push(copy.clone());
                false
            } else {
                true
            }
        });

        for copy in mine {
            // The client went away between asking and being served, which is
            // ordinary: a screenshot tool that was killed mid-copy.
            if !copy.frame.is_alive() {
                continue;
            }
            match self.copy_one(output, &copy, renderer) {
                Ok(()) => crate::screencopy::finish(&copy.frame, copy.region, copy.with_damage),
                Err(e) => {
                    tracing::warn!("screencopy failed: {e}");
                    copy.frame.failed();
                }
            }
        }
    }

    /// Serve every capture frame waiting on `output`.
    ///
    /// The same arrangement as screencopy: the copy happens where the renderer
    /// is, which is inside a backend, so the request only queues.
    pub fn service_image_capture<R, B>(&mut self, output: &Output, renderer: &mut R)
    where
        R: Renderer
            + Bind<B>
            + Bind<smithay::backend::allocator::dmabuf::Dmabuf>
            + Offscreen<B>
            + ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        if self.pending_capture_frames.is_empty() {
            return;
        }
        // A window's frame is served on the pass for a screen it is on, the
        // same rule the screencast path uses: this runs once per output and a
        // window has to be picked up by exactly one of those passes, or it is
        // either drawn twice or never.
        let mut mine = Vec::new();
        let mut rest = Vec::new();
        for (target, frame) in std::mem::take(&mut self.pending_capture_frames) {
            let ours = match &target {
                CaptureTarget::Output(frame_output) => frame_output == output,
                CaptureTarget::Window(id) => self.window_is_on(*id, output),
            };
            if ours {
                mine.push((target, frame));
            } else {
                rest.push((target, frame));
            }
        }
        self.pending_capture_frames = rest;

        let mut windows = Vec::new();
        let mut outputs = Vec::new();
        for (target, frame) in mine {
            match target {
                CaptureTarget::Output(frame_output) => outputs.push((frame_output, frame)),
                CaptureTarget::Window(id) => windows.push((id, frame)),
            }
        }

        for (id, frame) in windows {
            let buffer = frame.buffer();
            let result = match smithay::wayland::dmabuf::get_dmabuf(&buffer) {
                Ok(dmabuf) => self.render_window_into(id, dmabuf.clone(), renderer),
                Err(_) => self.copy_window_into::<R, B>(id, &buffer, renderer),
            };
            match result {
                Ok(()) => {
                    tracing::debug!("image capture: a frame of view {id}");
                    let now = self.start_time.elapsed();
                    // Normal, unlike an output. A window is not rotated by the
                    // screen it happens to be on — `read_window_pixels` draws
                    // it upright — so telling a client the screen's transform
                    // would have it turn an already-upright picture.
                    frame.success(smithay::utils::Transform::Normal, None, now);
                }
                Err(e) => {
                    tracing::warn!("image capture of view {id} failed: {e}");
                    frame.fail(
                        smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason::Unknown,
                    );
                }
            }
        }

        for (frame_output, frame) in outputs {
            let size = frame_output
                .current_mode()
                .map(|mode| frame_output.current_transform().transform_size(mode.size))
                .unwrap_or_default();
            let region = smithay::utils::Rectangle::from_size((size.w, size.h).into());
            let buffer = frame.buffer();
            // The cursor is a separate session in this protocol — a client
            // that wants it asks for it — so the copy of the output has none
            // in it.
            //
            // A dmabuf is drawn into directly. That is the whole reason a
            // recorder wants this protocol: the shared-memory path reads every
            // pixel back across the bus for each frame, which is affordable
            // once for a screenshot and not sixty times a second for a video.
            let result = match smithay::wayland::dmabuf::get_dmabuf(&buffer) {
                Ok(dmabuf) => {
                    self.render_output_into(&frame_output, dmabuf.clone(), false, renderer)
                }
                Err(_) => {
                    self.copy_output_into::<R, B>(&frame_output, region, false, &buffer, renderer)
                }
            };
            match result {
                Ok(()) => {
                    // Debug, not info: a recorder asks sixty times a second.
                    tracing::debug!("image capture: a frame of {}", frame_output.name());
                    let now = self.start_time.elapsed();
                    // The output's own transform, not Normal. The copy is
                    // composited the way the output is composited, so a client
                    // that is told Normal on a rotated or flipped monitor
                    // writes out an upside-down picture — which is exactly
                    // what the nested backend, whose output is flipped,
                    // produced.
                    //
                    // No damage: composited fresh, so the whole thing is new.
                    frame.success(frame_output.current_transform(), None, now);
                }
                Err(e) => {
                    tracing::warn!("image capture failed: {e}");
                    frame.fail(
                        smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason::Unknown,
                    );
                }
            }
        }
    }

    /// Composite an output straight into a buffer the client allocated.
    ///
    /// No readback: the frame is rendered where it is going to be read from,
    /// which is what makes recording at a screen's refresh rate possible at
    /// all.
    fn render_output_into<R>(
        &mut self,
        output: &Output,
        mut target: smithay::backend::allocator::dmabuf::Dmabuf,
        overlay_cursor: bool,
        renderer: &mut R,
    ) -> Result<(), String>
    where
        R: Renderer
            + Bind<smithay::backend::allocator::dmabuf::Dmabuf>
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        let mut frame = self.frame_for(output);
        if !overlay_cursor {
            frame.cursor = crate::render::Cursor::Hidden;
        }

        let size = output
            .current_mode()
            .map(|mode| output.current_transform().transform_size(mode.size))
            .ok_or_else(|| "the output has no mode".to_owned())?;
        let elements = crate::render::build(&frame, renderer);

        let mut framebuffer = renderer
            .bind(&mut target)
            .map_err(|e| format!("binding the client's buffer: {e}"))?;
        // From the output, so a rotated screen is drawn into the client's
        // buffer the way it is displayed rather than the way it is laid out.
        let mut tracker = smithay::backend::renderer::damage::OutputDamageTracker::new(
            size,
            output.current_scale().fractional_scale(),
            smithay::utils::Transform::Normal,
        );
        let result = tracker
            .render_output(
                renderer,
                &mut framebuffer,
                0,
                &elements,
                smithay::backend::renderer::Color32F::from([0.0, 0.0, 0.0, 1.0]),
            )
            .map_err(|e| format!("compositing into the client's buffer: {e:?}"))?;

        // Wait for the GPU to finish before the client is told the frame is
        // ready.
        //
        // Rendering is asynchronous: `render_output` returns once the work is
        // submitted, not once it is done. The shared-memory path reads the
        // result back, which waits by itself; this one hands the client the
        // buffer the GPU is still writing into, so a recorder that reads it
        // immediately sees whatever was there before — an untouched buffer,
        // which is black. That is a screen share of a black rectangle at the
        // right resolution and the right frame rate.
        if let Err(e) = result.sync.wait() {
            return Err(format!("waiting for the capture to finish: {e}"));
        }
        Ok(())
    }

    fn copy_one<R, B>(
        &mut self,
        output: &Output,
        copy: &PendingCopy,
        renderer: &mut R,
    ) -> Result<(), String>
    where
        R: Renderer
            + Bind<B>
            + Offscreen<B>
            + ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        self.copy_output_into::<R, B>(
            output,
            copy.region,
            copy.overlay_cursor,
            &copy.buffer,
            renderer,
        )
    }

    /// Composite `output` and write `region` of it into a client's shared
    /// memory buffer.
    ///
    /// Shared by both capture protocols. They disagree about how a client asks
    /// and how it is told, and not at all about what a screenshot is.
    pub fn copy_output_into<R, B>(
        &mut self,
        output: &Output,
        region: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
        overlay_cursor: bool,
        buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
        renderer: &mut R,
    ) -> Result<(), String>
    where
        R: Renderer
            + Bind<B>
            + Offscreen<B>
            + ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        let pixels = self.read_output_pixels::<R, B>(output, region, overlay_cursor, renderer)?;

        // Into the client's own memory. The shm path is the only one a client
        // can read without having allocated the buffer itself.
        smithay::wayland::shm::with_buffer_contents_mut(buffer, |ptr, len, data| {
            let want = (region.size.w * region.size.h * 4) as usize;
            if len < want || data.width < region.size.w || data.height < region.size.h {
                return Err(format!(
                    "the client's buffer is {}x{} and the copy is {}x{}",
                    data.width, data.height, region.size.w, region.size.h
                ));
            }
            // Row by row, because the client's stride need not be the packed
            // width — and writing as though it were shears the image.
            let stride = data.stride as usize;
            let row = (region.size.w * 4) as usize;
            for y in 0..region.size.h as usize {
                let from = &pixels[y * row..(y + 1) * row];
                // SAFETY: the length was checked above, and shm guarantees the
                // mapping is valid for the duration of this closure.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        from.as_ptr(),
                        ptr.add(data.offset as usize + y * stride),
                        row,
                    );
                }
            }
            Ok(())
        })
        .map_err(|e| format!("the client did not give shared memory: {e}"))??;

        Ok(())
    }

    /// Composite an output and read it back, packed, four bytes to a pixel.
    ///
    /// The shared half of every capture that cannot be drawn into directly: a
    /// client's shared memory, and a PipeWire buffer.
    pub fn read_output_pixels<R, B>(
        &mut self,
        output: &Output,
        region: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
        overlay_cursor: bool,
        renderer: &mut R,
    ) -> Result<Vec<u8>, String>
    where
        R: Renderer
            + Bind<B>
            + Offscreen<B>
            + ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        let mut frame = self.frame_for(output);
        if !overlay_cursor {
            // A screenshot with a pointer in it is rarely what was asked for,
            // and the client says which it wants.
            frame.cursor = crate::render::Cursor::Hidden;
        }

        let size = output
            .current_mode()
            .map(|mode| output.current_transform().transform_size(mode.size))
            .ok_or_else(|| "the output has no mode".to_owned())?;

        let elements = crate::render::build(&frame, renderer);
        // What went into the copy. A capture that comes back black is either a
        // frame with nothing in it or a frame that was drawn and read back
        // wrong, and the picture alone cannot say which.
        tracing::debug!(
            "capture of {}: {} element(s), {} window(s), shell {}",
            output.name(),
            elements.len(),
            frame.windows.len(),
            if frame.shell.is_some() { "yes" } else { "no" }
        );

        let buffer_size: smithay::utils::Size<i32, smithay::utils::Buffer> =
            (size.w, size.h).into();
        // Allocated in the format it will be read back as, because a renderer
        // is entitled to refuse to convert while copying and the Vulkan one
        // does: "cannot convert DrmFourcc(AR24) to DrmFourcc(XR24) while
        // copying" is what every capture on real hardware said, while the
        // nested GLES renderer converted quietly and hid it.
        //
        // XRGB either way, which is what a client is offered: a screenshot has
        // no transparency to carry, and a client that read the fourth byte as
        // alpha would show the whole image as see-through.
        let format = smithay::backend::allocator::Fourcc::Xrgb8888;
        let mut target = renderer
            .create_buffer(format, buffer_size)
            .map_err(|e| format!("allocating a copy target: {e}"))?;

        let mapping = {
            let mut framebuffer = renderer
                .bind(&mut target)
                .map_err(|e| format!("binding the copy target: {e}"))?;
            // From the output, so the copy carries its scale and its
            // transform. Hand-rolling it as (mode size, 1.0, Normal)
            // composites the desktop in the output's logical space — portrait,
            // for a rotated screen — and writes it into a landscape buffer
            // without turning it, which is a screenshot lying on its side.
            let mut tracker = smithay::backend::renderer::damage::OutputDamageTracker::new(
                size,
                output.current_scale().fractional_scale(),
                smithay::utils::Transform::Normal,
            );
            tracker
                .render_output(
                    renderer,
                    &mut framebuffer,
                    0,
                    &elements,
                    smithay::backend::renderer::Color32F::from([0.0, 0.0, 0.0, 1.0]),
                )
                .map_err(|e| format!("compositing the copy: {e:?}"))?;

            renderer
                .copy_framebuffer(
                    &framebuffer,
                    smithay::utils::Rectangle::new(
                        (region.loc.x, region.loc.y).into(),
                        (region.size.w, region.size.h).into(),
                    ),
                    format,
                )
                .map_err(|e| format!("reading the copy back: {e}"))?
        };
        let pixels = renderer
            .map_texture(&mapping)
            .map_err(|e| format!("mapping the copy: {e}"))?
            .to_vec();
        Ok(pixels)
    }

    /// Every output, as wlr-output-management needs to describe it.
    pub fn heads(&self) -> Vec<crate::output_management::Head> {
        // Enabled outputs are the ones in the space; a disabled one keeps its
        // CRTC but is unmapped, because the shell places windows from the
        // layout and a monitor that is off has no place in it.
        let mut heads: Vec<crate::output_management::Head> = self
            .space
            .outputs()
            .map(|output| crate::output_management::Head {
                output: output.clone(),
                enabled: true,
                position: self
                    .space
                    .output_geometry(output)
                    .map(|geometry| geometry.loc)
                    .unwrap_or_default(),
                adaptive_sync: self.adaptive_sync,
            })
            .collect();

        if let Some(udev) = self.udev.as_ref() {
            for surface in udev.surfaces().filter(|surface| !surface.enabled) {
                heads.push(crate::output_management::Head {
                    output: surface.output.clone(),
                    enabled: false,
                    position: Point::default(),
                    adaptive_sync: false,
                });
            }
        }
        heads
    }

    /// Tell every output-management client what the outputs are now.
    ///
    /// Deliberately not called from `notify_output_layout`: that fires when a
    /// layer surface changes the usable area, which is not an output change,
    /// and every call invalidates the configurations clients are holding.
    pub fn advertise_outputs(&mut self) {
        let heads = self.heads();
        let dh = self.display_handle.clone();
        self.output_management_state.advertise::<Self>(&dh, &heads);
    }

    /// Carry out — or check — what a client asked of the outputs.
    ///
    /// Everything is validated before anything is changed. A configuration is
    /// one operation to the client that sent it, and half of it applied is a
    /// layout nobody asked for: the monitor moved and the resolution refused.
    pub fn apply_output_configuration(
        &mut self,
        changes: &[crate::output_management::HeadChange],
        test_only: bool,
    ) -> bool {
        use std::collections::HashSet;

        let mut still_on: HashSet<String> = self
            .heads()
            .into_iter()
            .filter(|head| head.enabled)
            .map(|head| head.output.name())
            .collect();

        for change in changes {
            let Some(output) = self.any_output_by_name(&change.name) else {
                tracing::warn!("output configuration names {}, which is gone", change.name);
                return false;
            };
            if change.enabled {
                still_on.insert(change.name.clone());
            } else {
                still_on.remove(&change.name);
            }

            if let Some(mode) = change.mode {
                if mode.size.w <= 0 || mode.size.h <= 0 {
                    return false;
                }
                // A mode the display never offered cannot be programmed on
                // real hardware: the kernel takes a modeline from the
                // connector's own list. Nested has no such constraint, so a
                // custom mode is only refused where it would actually fail.
                let known = output.modes().contains(&mode);
                if !known && self.udev.is_some() {
                    tracing::warn!(
                        "{}: {}x{}@{} is not a mode this display offers",
                        change.name,
                        mode.size.w,
                        mode.size.h,
                        mode.refresh
                    );
                    return false;
                }
            }
            if change.scale.is_some_and(|scale| scale <= 0.0) {
                return false;
            }
        }

        // A session with every screen off cannot be turned back on from
        // inside it. Refusing is the only thing that leaves the user a way
        // back.
        if still_on.is_empty() {
            tracing::warn!("refusing a configuration that would turn every output off");
            return false;
        }

        if test_only {
            return true;
        }

        for change in changes {
            let Some(output) = self.any_output_by_name(&change.name) else {
                continue;
            };
            if !change.enabled {
                self.set_output_enabled(&output, false);
                continue;
            }
            self.set_output_enabled(&output, true);

            if let Some(mode) = change.mode {
                self.set_output_mode(&output, mode);
            }
            if change.transform.is_some() || change.scale.is_some() {
                let scale = change.scale.map(smithay::output::Scale::Fractional);
                output.change_current_state(None, change.transform, scale, None);
                self.output_reshaped(&output);
            }
            if let Some(position) = change.position {
                self.map_output_at(&output, (position.x, position.y));
            }
            if let Some(vrr) = change.adaptive_sync {
                self.set_output_adaptive_sync(&output, vrr);
            }
        }

        // Put every window that should be on screen back in the space.
        //
        // A window is in the space because the shell placed it there, and the
        // shell places from the layout — so a monitor coming back leaves any
        // window that was on it in whatever state it was left in, and nothing
        // re-sends a rectangle for a window whose rectangle has not changed.
        // The shell keeps drawing its frame either way, which is what a
        // re-enabled output showing borders and no windows was.
        self.remap_placed_views();

        self.notify_output_layout();
        self.advertise_outputs();
        self.needs_render = true;
        true
    }

    /// Every view the shell has placed and not hidden belongs in the space.
    ///
    /// Idempotent: mapping an element that is already mapped at the same
    /// position is what `Space::map_element` does anyway, so this can be run
    /// after anything that may have taken windows out.
    pub fn remap_placed_views(&mut self) {
        let placed: Vec<(smithay::desktop::Window, (i32, i32))> = self
            .views
            .iter()
            .filter(|view| view.mapped && view.visible && view.placed)
            .map(|view| (view.window.clone(), (view.box_.x, view.box_.y)))
            .collect();
        let count = placed.len();
        for (window, location) in placed {
            self.space.map_element(window, location, false);
        }
        // Same as a layout: mapping restacks, so focus decides what is on top,
        // and the floats stay above whatever that is.
        if let Some(window) = self.views.get(self.focused).map(|view| view.window.clone()) {
            self.space.raise_element(&window, false);
        }
        self.restack();
        // Said out loud because "the windows did not come back" and "the
        // windows came back somewhere off screen" look identical from a chair
        // in front of the monitor.
        tracing::info!(
            "re-placed {count} view(s); the space holds {}",
            self.space.elements().count()
        );
    }

    /// Put an output at a position, in the layout and in what clients are told.
    ///
    /// `Space::map_output` alone moves the output for the compositor's own
    /// layout and leaves `wl_output.geometry` saying whatever it said before,
    /// which for every output here was the `(0, 0)` it was created at. A client
    /// asking where the monitors are then gets them all stacked on the origin.
    ///
    /// There is no xdg-output global to paper over it either, so `wl_output` is
    /// the only answer a client has. mpv reads it to work out which screen it is
    /// on and where to go fullscreen; with two monitors both claiming the origin
    /// it picks by the accident of enumeration order.
    pub fn map_output_at(&mut self, output: &Output, location: impl Into<Point<i32, Logical>>) {
        let location = location.into();
        self.space.map_output(output, location);
        output.change_current_state(None, None, None, Some(location));
    }

    /// Program a mode on the hardware, not only in the description of it.
    ///
    /// `change_current_state` alone moves what every client is told and leaves
    /// the CRTC scanning out what it was: the windows resize and the picture
    /// does not.
    fn set_output_mode(&mut self, output: &Output, mode: smithay::output::Mode) {
        output.change_current_state(Some(mode), None, None, None);

        let Some(udev) = self.udev.as_mut() else {
            // Nested, where the mode is the host window's to decide.
            return;
        };
        let Some((id, connector)) = udev
            .outputs()
            .find(|(_, surface)| surface.output == *output)
            .map(|(crtc, surface)| (crtc, surface.connector))
        else {
            return;
        };

        // The kernel takes a modeline from the connector's own list rather
        // than numbers, so the one it offered has to be found again.
        //
        // Asked of the device this output is on. A connector handle is
        // device-local, exactly as a crtc handle is, and `id` names the device
        // because that is what makes it meaningful — so looking one up on the
        // primary is asking the wrong card about a connector it does not have.
        // What that gives is either a lookup that fails, and a mode change
        // that silently does nothing, or a handle that happens to be valid
        // there too and describes a different monitor entirely.
        use smithay::reexports::drm::control::Device as _;
        let Some(gpu) = udev.devices.get_mut(id.device) else {
            return;
        };
        let device = gpu.manager.device();
        let Ok(info) = device.get_connector(connector, false) else {
            return;
        };
        let Some(drm_mode) = info
            .modes()
            .iter()
            .copied()
            .find(|candidate| smithay::output::Mode::from(*candidate) == mode)
        else {
            tracing::warn!("{}: the display no longer offers that mode", output.name());
            return;
        };

        let Some(device) = udev.devices.get_mut(id.device) else {
            return;
        };
        let Some(surface) = device.surfaces.get_mut(&id.crtc) else {
            return;
        };
        // No render elements: this is a modeset, and the frame after it is
        // drawn by the ordinary loop. Passing the current ones would only
        // matter for keeping other outputs lit through a bandwidth
        // renegotiation, and they are redrawn a moment later anyway.
        let result = crate::with_gpu!(&mut device.renderer, |renderer| surface
            .drm_output
            .use_mode(
                drm_mode,
                renderer,
                &smithay::backend::drm::output::DrmOutputRenderElements::<
                    _,
                    crate::render::OutputElement<_>,
                >::new(),
            )
            .map_err(|e| e.to_string()));
        match result {
            Ok(()) => tracing::info!(
                "{}: {}x{}@{}",
                output.name(),
                mode.size.w,
                mode.size.h,
                mode.refresh
            ),
            Err(e) => tracing::warn!("{}: the display refused the mode: {e}", output.name()),
        }
        // A modeset invalidates what was queued for this output.
        surface.pending = false;

        // And a different mode is a different screen, so the layer map and the
        // damage history are as stale as they are after a rotation.
        self.output_reshaped(output);
    }

    /// Turn one output on or off.
    ///
    /// The surface and its CRTC are kept either way, so coming back is a commit
    /// rather than a re-scan of the device. Off means the planes are cleared
    /// rather than painted black: a black frame still lights the panel.
    pub(crate) fn set_output_enabled(&mut self, output: &Output, enabled: bool) {
        let mapped = self.space.outputs().any(|other| other == output);
        if enabled == mapped {
            let already = self
                .udev
                .as_ref()
                .map(|udev| {
                    udev.surfaces()
                        .find(|surface| surface.output == *output)
                        .map(|surface| surface.enabled == enabled)
                        .unwrap_or(true)
                })
                .unwrap_or(true);
            if already {
                return;
            }
        }

        if enabled {
            // Back where it was is not knowable — an unmapped output has no
            // geometry — so it goes to the right of everything, which is where
            // a newly plugged monitor goes too.
            let x = self
                .space
                .outputs()
                .filter_map(|other| self.space.output_geometry(other))
                .map(|geometry| geometry.loc.x + geometry.size.w)
                .max()
                .unwrap_or(0);
            self.map_output_at(output, (x, 0));
        } else {
            self.space.unmap_output(output);
        }

        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        let Some(surface) = udev
            .surfaces_mut()
            .find(|surface| surface.output == *output)
        else {
            return;
        };
        surface.enabled = enabled;
        surface.pending = false;
        if enabled {
            // The damage history describes a screen that has since been
            // cleared, so the next frame would redraw only what changed while
            // it was off — which for a still desktop is nothing, and the
            // monitor comes back showing the wallpaper with no windows on it.
            surface.drm_output.reset_buffers();
        }
        if !enabled {
            if let Err(e) = surface
                .drm_output
                .with_compositor(|compositor| compositor.clear())
            {
                tracing::warn!("could not switch {} off: {e}", output.name());
            }
        }
        tracing::info!("{} {}", output.name(), if enabled { "on" } else { "off" });
    }

    /// Variable refresh on one output.
    fn set_output_adaptive_sync(&mut self, output: &Output, enabled: bool) {
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        let Some(surface) = udev
            .surfaces_mut()
            .find(|surface| surface.output == *output)
        else {
            return;
        };
        match surface
            .drm_output
            .with_compositor(|compositor| compositor.use_vrr(enabled))
        {
            Ok(()) => tracing::info!(
                "adaptive sync {} on {}",
                if enabled { "on" } else { "off" },
                output.name()
            ),
            // Most panels cannot, and asking is how you find out.
            Err(e) => tracing::debug!("adaptive sync unavailable on {}: {e}", output.name()),
        }
    }

    /// How many gamma entries this output's CRTC takes.
    ///
    /// `None` where there is no CRTC to ask — the nested backend, or a driver
    /// that offers no ramp — which tells a night-light client to skip this
    /// monitor rather than wait for a ramp that will never take.
    pub fn output_gamma_size(&mut self, output: &Output) -> Option<u32> {
        use smithay::reexports::drm::control::Device as _;

        let udev = self.udev.as_ref()?;
        let id = udev.id_of(output)?;
        let length = udev
            .devices
            .get(id.device)?
            .manager
            .device()
            .get_crtc(id.crtc)
            .ok()?
            .gamma_length();
        (length > 0).then_some(length)
    }

    /// Put a ramp on an output, or take one off.
    ///
    /// The legacy ioctl rather than the atomic GAMMA_LUT property: it is one
    /// call that does not have to join the commit putting a frame on screen,
    /// and a gamma change that waited for a page flip would be a colour shift
    /// that only lands when something moves.
    pub fn set_output_gamma(&mut self, output: &Output, ramp: Option<&crate::gamma::Ramp>) -> bool {
        let name = output.name();
        match ramp {
            Some(ramp) => {
                self.gamma_ramps.insert(name.clone(), ramp.clone());
            }
            None => {
                self.gamma_ramps.remove(&name);
            }
        }

        let Some(size) = self.output_gamma_size(output) else {
            return false;
        };
        let identity;
        let ramp = match ramp {
            Some(ramp) => ramp,
            None => {
                // Straight through, which is what a display with no client
                // looking after it should show. Leaving the last ramp in place
                // means a night-light client that was killed leaves the screen
                // orange until the next reboot.
                identity = crate::gamma::identity(size as usize);
                &identity
            }
        };
        self.apply_gamma(output, ramp)
    }

    fn apply_gamma(&mut self, output: &Output, ramp: &crate::gamma::Ramp) -> bool {
        use smithay::reexports::drm::control::Device as _;

        let Some(udev) = self.udev.as_ref() else {
            return false;
        };
        let Some(id) = udev.id_of(output) else {
            return false;
        };
        let Some(device) = udev.devices.get(id.device) else {
            return false;
        };
        match device
            .manager
            .device()
            .set_gamma(id.crtc, &ramp.red, &ramp.green, &ramp.blue)
        {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("{}: the gamma ramp was refused: {e}", output.name());
                false
            }
        }
    }

    /// Put every ramp back after a VT switch.
    ///
    /// The kernel resets gamma when the session is handed over, and the client
    /// that set it has no way to know that happened — so coming back from
    /// another VT would drop the screen out of night mode until the next time
    /// wlsunset happened to recalculate.
    pub fn restore_gamma(&mut self) {
        let ramps: Vec<(String, crate::gamma::Ramp)> = self
            .gamma_ramps
            .iter()
            .map(|(name, ramp)| (name.clone(), ramp.clone()))
            .collect();
        for (name, ramp) in ramps {
            let Some(output) = self.any_output_by_name(&name) else {
                continue;
            };
            self.apply_gamma(&output, &ramp);
        }
    }

    /// Whether the focused client has asked for the compositor's own chords.
    ///
    /// A virtual machine and a remote desktop both need Mod4 to reach the
    /// session inside them rather than the one around it. The inhibitor is per
    /// surface, so this is only true while that surface holds the keyboard —
    /// the chords come back the moment focus leaves it.
    pub fn shortcuts_inhibited(&self) -> bool {
        use smithay::utils::IsAlive as _;

        let Some(keyboard) = self.seat.get_keyboard() else {
            return false;
        };
        let Some(focus) = keyboard.current_focus() else {
            return false;
        };
        self.shortcut_inhibitors.iter().any(|inhibitor| {
            inhibitor.wl_surface().alive()
                && *inhibitor.wl_surface() == focus
                && inhibitor.is_active()
        })
    }

    /// What a client may allocate a capture buffer as.
    ///
    /// `None` on the nested backend: there is no DRM node of this
    /// compositor's to name, and a client cannot allocate against one it
    /// cannot open.
    pub fn capture_dmabuf_constraints(
        &self,
    ) -> Option<smithay::wayland::image_copy_capture::DmabufConstraints> {
        use smithay::backend::allocator::Fourcc;

        let (node, formats_in) = self.capture_gpu.as_ref()?;
        let node = *node;

        let mut formats: std::collections::HashMap<Fourcc, Vec<_>> =
            std::collections::HashMap::new();
        for format in formats_in {
            // The two a capture is ever asked for. Offering everything the
            // renderer can import would have a client allocate a format the
            // compositor cannot then draw into.
            if !matches!(format.code, Fourcc::Xrgb8888 | Fourcc::Argb8888) {
                continue;
            }
            formats
                .entry(format.code)
                .or_default()
                .push(format.modifier);
        }
        if formats.is_empty() {
            return None;
        }
        Some(smithay::wayland::image_copy_capture::DmabufConstraints {
            node,
            formats: formats.into_iter().collect(),
        })
    }

    /// Whether this output's frames may tear.
    ///
    /// Only when one window covers the whole of it and that window's client
    /// asked. A torn flip tears the screen, not a window: a game asking for it
    /// while a terminal and a bar share the monitor would tear those too, and
    /// neither asked. The shell's own buffer is behind a covering window and
    /// never visible, so it does not count against this.
    pub fn output_wants_tearing(&self, output: &Output) -> bool {
        use smithay::wayland::seat::WaylandFocus as _;

        if self.locked || self.overview {
            return false;
        }
        let Some(area) = self.space.output_geometry(output) else {
            return false;
        };
        // Anything layered on this output — a bar, a launcher — is drawn over
        // the window and would tear with it.
        if smithay::desktop::layer_map_for_output(output)
            .layers()
            .next()
            .is_some()
        {
            return false;
        }

        let mut covering = None;
        for window in self.space.elements() {
            let Some(geometry) = self.space.element_geometry(window) else {
                continue;
            };
            if !area.overlaps(geometry) {
                continue;
            }
            // A second window on the same output: whatever the first asked
            // for, the second did not.
            if covering.is_some() {
                return false;
            }
            if !geometry.contains_rect(area) {
                return false;
            }
            covering = Some(window);
        }

        covering
            .and_then(|window| window.wl_surface().map(|surface| surface.into_owned()))
            .map(|surface| self.tearing_state.wants_tearing(&surface))
            .unwrap_or(false)
    }

    /// Turn one monitor's backlight off, or on again.
    ///
    /// DPMS rather than removing the output: a monitor that is asleep is still
    /// where it was, so windows stay on it and come back when it wakes. That
    /// is the difference between this and disabling an output through
    /// wlr-output-management, which takes it out of the layout.
    pub fn set_output_power(&mut self, output: &Output, on: bool) {
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        let Some(surface) = udev
            .surfaces_mut()
            .find(|surface| surface.output == *output)
        else {
            return;
        };
        if surface.powered == on {
            return;
        }
        surface.powered = on;
        surface.pending = false;

        if on {
            // Everything that was on screen went with the blanking and the
            // damage history does not know it.
            surface.drm_output.reset_buffers();
            self.needs_render = true;
        } else if let Err(e) = surface
            .drm_output
            .with_compositor(|compositor| compositor.clear())
        {
            tracing::warn!("could not turn {} off: {e}", output.name());
        }
        tracing::info!("{}: {}", output.name(), if on { "on" } else { "off" });

        let on = self.output_powered(output);
        let state = std::mem::take(&mut self.output_power_state);
        state.changed(output, on);
        self.output_power_state = state;
    }

    /// Whether a monitor's backlight is on.
    pub fn output_powered(&self, output: &Output) -> bool {
        self.udev
            .as_ref()
            .and_then(|udev| {
                udev.surfaces()
                    .find(|surface| surface.output == *output)
                    .map(|surface| surface.powered)
            })
            // Nested and headless have nothing to turn off, and saying a
            // monitor is on is the truthful answer for a window.
            .unwrap_or(true)
    }

    /// Start sharing an output, and say which PipeWire node to watch.
    ///
    /// The connection is made on the first request rather than at startup: a
    /// desktop nobody is sharing has no reason to hold one open, and a session
    /// without PipeWire should still be a working desktop.
    pub fn start_cast(
        &mut self,
        source: crate::screencast::Source,
    ) -> anyhow::Result<(u32, smithay::utils::Size<i32, smithay::utils::Physical>)> {
        if self.pipewire.is_none() {
            self.pipewire = Some(crate::screencast::stream::Pipewire::new()?);
        }

        let (name, size) = match &source {
            crate::screencast::Source::Output(output) => {
                let size = output
                    .current_mode()
                    .map(|mode| mode.size)
                    .ok_or_else(|| anyhow::anyhow!("that output has no mode"))?;
                (output.name(), size)
            }
            crate::screencast::Source::Window(id) => {
                let view = self
                    .views
                    .get(*id)
                    .ok_or_else(|| anyhow::anyhow!("no such window"))?;
                let size = self
                    .space
                    .element_geometry(&view.window)
                    .map(|geometry| geometry.size)
                    .ok_or_else(|| anyhow::anyhow!("that window is not on screen"))?;
                (view.title(), (size.w, size.h).into())
            }
        };

        // Buffers the GPU can draw into, if this backend can allocate any.
        // Without them the stream falls back to shared memory, which costs a
        // whole screen off the GPU and back for every frame.
        let targets = self.cast_targets(size);
        let pipewire = self.pipewire.as_ref().expect("just connected");
        let stream = pipewire.create_stream(&name, size, targets)?;
        let node = stream.node_id;
        self.casts.push(crate::screencast::Cast { source, stream });
        tracing::info!("sharing {name} as pipewire node {node}");
        Ok((node, size))
    }

    /// Allocate the buffers a stream will hand out.
    ///
    /// All of them or none: a stream with some of its buffers is one that
    /// stutters between the two paths, and the shared-memory fallback works
    /// whole.
    ///
    /// Only on the DRM backend. It is the one with a GPU allocator, and it is
    /// also the only one anybody shares a screen from — nested and headless
    /// are for testing, and both still stream through shared memory.
    fn cast_targets(
        &mut self,
        size: smithay::utils::Size<i32, smithay::utils::Physical>,
    ) -> Vec<smithay::backend::allocator::dmabuf::Dmabuf> {
        // Through the backend's own renderer, which is where the allocator
        // lives. Only reachable from outside the render path — see
        // `allocate_cast_targets`.
        let Some(mut udev) = self.udev.take() else {
            return Vec::new();
        };
        // DMA-BUF targets come from the Vulkan renderer's allocator; GLES has
        // no `Offscreen<Dmabuf>`, so a screen share under it takes the
        // shared-memory path instead of handing buffers over.
        let targets = match &mut udev.primary_mut().renderer {
            crate::udev::Gpu::Vulkan(renderer) => Self::allocate_cast_targets(renderer, size),
            _ => Vec::new(),
        };
        self.udev = Some(udev);
        targets
    }

    /// Allocate against a renderer that is already in hand.
    ///
    /// Taking the renderer rather than reaching for `self.udev`, because the
    /// render path has already moved it out of the state — it has to, to lend
    /// it out while calling back into the compositor. Reaching for it there
    /// found nothing and returned no buffers, and a stream with no buffers to
    /// offer advertises no DMA-BUF format at all: every renegotiation quietly
    /// dropped the share onto the shared-memory path, which is the readback
    /// per frame this was written to avoid. Nothing said so, because "could
    /// not allocate" and "this backend has no allocator" looked the same.
    fn allocate_cast_targets<R>(
        renderer: &mut R,
        size: smithay::utils::Size<i32, smithay::utils::Physical>,
    ) -> Vec<smithay::backend::allocator::dmabuf::Dmabuf>
    where
        R: Offscreen<smithay::backend::allocator::dmabuf::Dmabuf>,
    {
        let buffer_size: smithay::utils::Size<i32, smithay::utils::Buffer> =
            (size.w.max(1), size.h.max(1)).into();

        let mut targets = Vec::with_capacity(crate::screencast::stream::BUFFERS);
        for _ in 0..crate::screencast::stream::BUFFERS {
            // The same format the readback path used, which is what the stream
            // describes to the consumer: four bytes a pixel, no alpha, because
            // a screen is opaque and a consumer that reads the fourth byte as
            // alpha shows a transparent picture.
            match renderer.create_buffer(smithay::backend::allocator::Fourcc::Xrgb8888, buffer_size)
            {
                Ok(target) => targets.push(target),
                Err(e) => {
                    tracing::warn!("could not allocate a screencast buffer: {e}");
                    return Vec::new();
                }
            }
        }
        targets
    }

    /// Stop sharing whatever a session was showing.
    pub fn stop_cast(&mut self, node: u32) {
        let before = self.casts.len();
        self.casts.retain(|cast| cast.stream.node_id != node);
        if self.casts.len() != before {
            tracing::info!("stopped sharing on pipewire node {node}");
        }
        if self.casts.is_empty() {
            // Nothing is being shared, so the connection is not worth holding.
            self.pipewire = None;
        }
    }

    /// Hand this output's frame to anything sharing it.
    pub fn feed_casts<R, B>(&mut self, output: &Output, renderer: &mut R)
    where
        R: Renderer
            + Bind<B>
            + Bind<smithay::backend::allocator::dmabuf::Dmabuf>
            + Offscreen<B>
            + ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        if self.casts.is_empty() {
            return;
        }

        // What a share is worth asking the renderer for.
        //
        // Compositing and reading back a screen is a full frame off the GPU —
        // fifteen megabytes at 1440p — and doing it at the compositor's own
        // rate made the desktop lag while a share was open. Thirty a second
        // is what a screen share is watched at.
        const RATE: std::time::Duration = std::time::Duration::from_millis(33);
        if !self.casts.iter().any(|cast| cast.stream.wants_frame(RATE)) {
            return;
        }

        // The streams that take a buffer the GPU drew into, first and one at a
        // time. Each is composited straight into the memory the consumer will
        // read, so there is nothing to share between them and nothing to copy.
        self.draw_into_casts(output, renderer);

        // Then the ones that need pixels in shared memory. One composite and
        // one readback serves every client watching this output.
        let watching_output = self.casts.iter().any(|cast| {
            cast.stream.wants_frame(RATE)
                && !cast.stream.uses_dmabuf()
                && matches!(&cast.source, crate::screencast::Source::Output(o) if o == output)
        });
        if watching_output {
            if let Some(size) = output
                .current_mode()
                .map(|mode| output.current_transform().transform_size(mode.size))
            {
                let region = smithay::utils::Rectangle::from_size((size.w, size.h).into());
                // The cursor is drawn in: this is a picture of a screen rather
                // than a screenshot of one, and a share without a pointer is
                // hard to follow.
                match self.read_output_pixels::<R, B>(output, region, true, renderer) {
                    Ok(pixels) => self.push_to_casts(
                        |source| {
                            matches!(source, crate::screencast::Source::Output(o) if o == output)
                        },

                        &pixels,
                        size,
                    ),
                    Err(e) => tracing::warn!("could not read a frame for a screencast: {e}"),
                }
            }
        }

        // Then windows, one composite each. A window is shared as itself
        // rather than as the part of the screen it covers: whatever is on top
        // of it belongs to the desktop, not to the thing being shared.
        let windows: Vec<u32> = self
            .casts
            .iter()
            .filter(|cast| cast.stream.wants_frame(RATE) && !cast.stream.uses_dmabuf())
            .filter_map(|cast| match &cast.source {
                crate::screencast::Source::Window(id) => Some(*id),
                _ => None,
            })
            .collect();
        for id in windows {
            let on_this_output = self
                .views
                .get(id)
                .and_then(|view| self.space.element_geometry(&view.window))
                .zip(self.space.output_geometry(output))
                .map(|(window, screen)| screen.overlaps(window))
                .unwrap_or(false);
            if !on_this_output {
                continue;
            }
            match self.read_window_pixels::<R, B>(id, renderer) {
                Ok((pixels, size)) => self.push_to_casts(
                    |source| matches!(source, crate::screencast::Source::Window(other) if *other == id),
                    &pixels,
                    size,
                ),
                Err(e) => tracing::warn!("could not read a window for a screencast: {e}"),
            }
        }

        // Keep drawing while anything is watching.
        //
        // Rendering is driven by damage, and a desktop nobody is touching
        // produces none — so the compositor drew one frame, handed it over,
        // and stopped. A share is a stream: the viewer needs a frame whether
        // or not this end has changed, and one that stops arriving reads as a
        // frozen screen rather than a still one.
        self.needs_render = true;
    }

    /// The size a source is now, whatever it was when the share started.
    fn cast_size(
        &self,
        source: &crate::screencast::Source,
    ) -> Option<smithay::utils::Size<i32, smithay::utils::Physical>> {
        match source {
            crate::screencast::Source::Output(output) => output
                .current_mode()
                .map(|mode| output.current_transform().transform_size(mode.size)),
            crate::screencast::Source::Window(id) => {
                let view = self.views.get(*id)?;
                let geometry = self.space.element_geometry(&view.window)?;
                Some((geometry.size.w.max(1), geometry.size.h.max(1)).into())
            }
        }
    }

    /// Agree a new format for anything whose source has resized.
    /// Called from the backend before it feeds them, because only the backend
    /// knows whether it can allocate: `None` renegotiates without DMA-BUFs,
    /// which is right for a nested session — its streams are shared memory
    /// anyway, and shared memory is allocated when PipeWire asks rather than
    /// up front.
    pub fn resize_casts<R>(&mut self, renderer: Option<&mut R>)
    where
        R: Offscreen<smithay::backend::allocator::dmabuf::Dmabuf>,
    {
        let mut renderer = renderer;
        let resized: Vec<(usize, smithay::utils::Size<i32, smithay::utils::Physical>)> = self
            .casts
            .iter()
            .enumerate()
            .filter_map(|(at, cast)| {
                let size = self.cast_size(&cast.source)?;
                cast.stream.needs_renegotiation(size).then_some((at, size))
            })
            .collect();
        if resized.is_empty() {
            return;
        }

        for (at, size) in resized {
            // Buffers of the new size, before the offer goes out: the consumer
            // may take the format at once and ask for them on its own thread.
            let targets = match renderer.as_deref_mut() {
                Some(renderer) => Self::allocate_cast_targets(renderer, size),
                None => Vec::new(),
            };
            let mut casts = std::mem::take(&mut self.casts);
            if let (Some(cast), Some(pipewire)) = (casts.get_mut(at), self.pipewire.as_ref()) {
                if let Err(e) = cast
                    .stream
                    .renegotiate(size, targets, &pipewire.thread_loop)
                {
                    tracing::warn!("could not resize a screencast: {e}");
                }
            }
            self.casts = casts;
        }
    }

    /// Composite a frame straight into the buffer each waiting stream will
    /// hand to its consumer.
    ///
    /// The point of the whole DMA-BUF path: the shared-memory one reads a
    /// screen back off the GPU and writes it out again — fifteen megabytes a
    /// frame at 1440p, thirty times a second — and this one draws where the
    /// consumer is already looking.
    fn draw_into_casts<R>(&mut self, output: &Output, renderer: &mut R)
    where
        R: Renderer
            + Bind<smithay::backend::allocator::dmabuf::Dmabuf>
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        const RATE: std::time::Duration = std::time::Duration::from_millis(33);

        // Both taken out for the duration: compositing needs the whole state,
        // and the stream being drawn into is part of it.
        let mut casts = std::mem::take(&mut self.casts);
        let pipewire = self.pipewire.take();
        if let Some(pipewire) = pipewire.as_ref() {
            for cast in casts.iter_mut() {
                if !cast.stream.uses_dmabuf() || !cast.stream.wants_frame(RATE) {
                    continue;
                }
                match &cast.source {
                    crate::screencast::Source::Output(shared) if shared == output => {
                        let shared = shared.clone();
                        let size = match shared.current_mode() {
                            Some(mode) => shared.current_transform().transform_size(mode.size),
                            None => continue,
                        };
                        cast.stream
                            .with_target(size, &pipewire.thread_loop, |target| {
                                // The cursor is drawn in: this is a picture of
                                // a screen rather than a screenshot of one, and
                                // a share without a pointer is hard to follow.
                                self.render_output_into(&shared, target.clone(), true, renderer)
                            });
                    }
                    crate::screencast::Source::Window(id) => {
                        let id = *id;
                        // Only from the output it is on, so a window straddling
                        // two screens is not composited once for each.
                        let geometry = self
                            .views
                            .get(id)
                            .and_then(|view| self.space.element_geometry(&view.window));
                        let on_this_output = geometry
                            .zip(self.space.output_geometry(output))
                            .map(|(window, screen)| screen.overlaps(window))
                            .unwrap_or(false);
                        let Some(geometry) = geometry.filter(|_| on_this_output) else {
                            continue;
                        };
                        let size = (geometry.size.w.max(1), geometry.size.h.max(1)).into();
                        cast.stream
                            .with_target(size, &pipewire.thread_loop, |target| {
                                self.render_window_into(id, target.clone(), renderer)
                            });
                    }
                    _ => {}
                }
            }
        }
        self.pipewire = pipewire;
        self.casts = casts;
    }

    /// Hand a frame to every cast a predicate matches.
    fn push_to_casts(
        &mut self,
        matches: impl Fn(&crate::screencast::Source) -> bool,
        pixels: &[u8],
        size: smithay::utils::Size<i32, smithay::utils::Physical>,
    ) {
        let mut casts = std::mem::take(&mut self.casts);
        if let Some(pipewire) = self.pipewire.as_ref() {
            for cast in casts
                .iter_mut()
                .filter(|cast| !cast.stream.uses_dmabuf() && matches(&cast.source))
            {
                cast.stream.push(pixels, size, &pipewire.thread_loop);
            }
        }
        self.casts = casts;
    }

    /// Whether a view is showing on this output at all.
    ///
    /// Overlap, not containment: a window straddling two screens is on both,
    /// and the caller picks one. False for a view that has gone — which is how
    /// a capture of a closed window stops being anybody's to serve.
    pub fn window_is_on(&self, id: u32, output: &Output) -> bool {
        self.views
            .get(id)
            .and_then(|view| self.space.element_geometry(&view.window))
            .zip(self.space.output_geometry(output))
            .map(|(window, screen)| screen.overlaps(window))
            .unwrap_or(false)
    }

    /// Composite one window and copy it into a client's shared memory.
    ///
    /// The shm half of `render_window_into`, and the same relationship
    /// `copy_output_into` has to `render_output_into`: a client that could not
    /// allocate a DMA-BUF still gets its picture, at the cost of reading every
    /// pixel back.
    fn copy_window_into<R, B>(
        &mut self,
        id: u32,
        buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
        renderer: &mut R,
    ) -> Result<(), String>
    where
        R: Renderer
            + Bind<B>
            + Offscreen<B>
            + ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        let (pixels, size) = self.read_window_pixels::<R, B>(id, renderer)?;

        smithay::wayland::shm::with_buffer_contents_mut(buffer, |ptr, len, data| {
            let want = (size.w * size.h * 4) as usize;
            if len < want || data.width < size.w || data.height < size.h {
                return Err(format!(
                    "the client's buffer is {}x{} and the window is {}x{}",
                    data.width, data.height, size.w, size.h
                ));
            }
            // Row by row: the client's stride need not be the packed width,
            // and writing as though it were shears the image.
            let stride = data.stride as usize;
            let row = (size.w * 4) as usize;
            for y in 0..size.h as usize {
                let from = &pixels[y * row..(y + 1) * row];
                // SAFETY: the length was checked above, and shm guarantees the
                // mapping is valid for the duration of this closure.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        from.as_ptr(),
                        ptr.add(data.offset as usize + y * stride),
                        row,
                    );
                }
            }
            Ok(())
        })
        .map_err(|e| format!("the client did not give shared memory: {e}"))??;

        Ok(())
    }

    /// Composite one window straight into a buffer a consumer will read.
    ///
    /// The same picture `read_window_pixels` produces, drawn where it is going
    /// rather than read back and copied.
    fn render_window_into<R>(
        &mut self,
        id: u32,
        mut target: smithay::backend::allocator::dmabuf::Dmabuf,
        renderer: &mut R,
    ) -> Result<(), String>
    where
        R: Renderer
            + Bind<smithay::backend::allocator::dmabuf::Dmabuf>
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        let (elements, size) = self.window_elements(id, renderer)?;

        let mut framebuffer = renderer
            .bind(&mut target)
            .map_err(|e| format!("binding a window capture target: {e}"))?;
        let mut tracker = smithay::backend::renderer::damage::OutputDamageTracker::new(
            size,
            1.0,
            smithay::utils::Transform::Normal,
        );
        let result = tracker
            .render_output(
                renderer,
                &mut framebuffer,
                0,
                &elements,
                smithay::backend::renderer::Color32F::from([0.0, 0.0, 0.0, 1.0]),
            )
            .map_err(|e| format!("compositing a window: {e:?}"))?;

        // Waited for, because nothing else will. Rendering returns once the
        // work is submitted, and a consumer handed the buffer the GPU is still
        // writing into reads whatever was there before.
        result
            .sync
            .wait()
            .map_err(|e| format!("waiting for a window capture to finish: {e}"))
    }

    /// One window's own surface tree, drawn at its own origin.
    ///
    /// Its own tree rather than the part of the screen it occupies: what is on
    /// top of a window belongs to the desktop, and a client that asked to share
    /// a window did not ask to share whatever is covering it. Drawn at the
    /// window's origin so the shadow a client draws outside its geometry falls
    /// off the edge rather than shifting the picture.
    fn window_elements<R>(&mut self, id: u32, renderer: &mut R) -> Result<WindowElements<R>, String>
    where
        R: Renderer + smithay::backend::renderer::ImportAll,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
    {
        use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
        use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
        use smithay::backend::renderer::element::Kind;
        use smithay::wayland::seat::WaylandFocus as _;

        let view = self
            .views
            .get(id)
            .ok_or_else(|| "no such window".to_owned())?;
        let window = view.window.clone();
        let geometry = window.geometry();
        let size: smithay::utils::Size<i32, smithay::utils::Physical> =
            (geometry.size.w.max(1), geometry.size.h.max(1)).into();
        let surface = window
            .wl_surface()
            .ok_or_else(|| "that window has no surface".to_owned())?
            .into_owned();

        let elements = render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<R>>(
            renderer,
            &surface,
            (-geometry.loc.x, -geometry.loc.y),
            1.0,
            1.0,
            Kind::Unspecified,
        );
        Ok((elements, size))
    }

    /// Composite one window on its own, and read it back.
    ///
    /// Its own surface tree rather than the part of the screen it occupies:
    /// what is on top of a window belongs to the desktop, and a client that
    /// asked to share a window did not ask to share whatever is covering it.
    fn read_window_pixels<R, B>(
        &mut self,
        id: u32,
        renderer: &mut R,
    ) -> Result<(Vec<u8>, smithay::utils::Size<i32, smithay::utils::Physical>), String>
    where
        R: Renderer
            + Bind<B>
            + Offscreen<B>
            + ExportMem
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
        <R as smithay::backend::renderer::RendererSuper>::Error: Send + Sync + 'static,
    {
        let (elements, size) = self.window_elements(id, renderer)?;

        let format = smithay::backend::allocator::Fourcc::Xrgb8888;
        let buffer_size: smithay::utils::Size<i32, smithay::utils::Buffer> =
            (size.w, size.h).into();
        let mut target = renderer
            .create_buffer(format, buffer_size)
            .map_err(|e| format!("allocating a window capture target: {e}"))?;

        let mapping = {
            let mut framebuffer = renderer
                .bind(&mut target)
                .map_err(|e| format!("binding a window capture target: {e}"))?;
            // A window, not an output: its own size, upright. A window is not
            // rotated by the screen it happens to be on — what a client asked
            // to capture is the window.
            let mut tracker = smithay::backend::renderer::damage::OutputDamageTracker::new(
                size,
                1.0,
                smithay::utils::Transform::Normal,
            );
            tracker
                .render_output(
                    renderer,
                    &mut framebuffer,
                    0,
                    &elements,
                    smithay::backend::renderer::Color32F::from([0.0, 0.0, 0.0, 1.0]),
                )
                .map_err(|e| format!("compositing a window: {e:?}"))?;
            renderer
                .copy_framebuffer(
                    &framebuffer,
                    smithay::utils::Rectangle::from_size(buffer_size),
                    format,
                )
                .map_err(|e| format!("reading a window back: {e}"))?
        };
        let pixels = renderer
            .map_texture(&mapping)
            .map_err(|e| format!("mapping a window capture: {e}"))?
            .to_vec();
        Ok((pixels, size))
    }

    /// Carry out what the portal asked for.
    pub fn handle_screencast(&mut self, message: crate::screencast::portal::Message) {
        use crate::screencast::portal::Message;

        match message {
            Message::Start { types, reply } => self.open_screencast_picker(types, reply),
            Message::Close { node } => self.stop_cast(node),
        }
    }

    /// How long a chooser stays up before it gives up on being answered.
    ///
    /// The application is waiting on this: its own dialogue says the share is
    /// starting for as long as the chooser is open, so a user who walked away
    /// leaves it there. Long enough to read the list and think about it.
    const PICK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

    /// Ask the user what to share.
    fn open_screencast_picker(
        &mut self,
        types: u32,
        reply: async_channel::Sender<Result<crate::screencast::portal::Started, String>>,
    ) {
        // One at a time. Two choosers on screen with one keyboard between them
        // is a race the user cannot see, let alone win.
        if self.picker.is_some() {
            let _ = reply.try_send(Err("something else is already being chosen".to_owned()));
            return;
        }

        let sources = self.screencast_sources(types);
        if sources.is_empty() {
            let _ = reply.try_send(Err("there is nothing to share".to_owned()));
            return;
        }

        // Nobody to draw it. A shell that is not up — a test, a crash, a build
        // without the web engine — should still be able to share a screen, so
        // this falls back to what was on screen when the user pressed share.
        if !self.shell_is_up() {
            let source = sources.into_iter().next().expect("checked above");
            let answer = self.begin_cast(source);
            let _ = reply.try_send(answer);
            return;
        }

        let id = self.next_pick;
        self.next_pick = self.next_pick.wrapping_add(1).max(1);
        self.picker = Some(crate::screencast::Picker {
            id,
            sources,
            selected: 0,
            restore: self.focused,
            reply,
        });

        // The keys have to come here rather than to whatever was focused: the
        // chooser is driven from the compositor, and a keystroke meant for it
        // that reached a terminal instead would be typed into it.
        if let Some(keyboard) = self.seat.get_keyboard() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Option::<WlSurface>::None, serial);
        }
        self.notify_picker();

        // Answered either way, in the end. An application left waiting on a
        // chooser nobody is looking at shows a share that is forever about to
        // start.
        let _ = self.loop_handle.insert_source(
            smithay::reexports::calloop::timer::Timer::from_duration(Self::PICK_TIMEOUT),
            move |_, _, state| {
                if state.picker.as_ref().is_some_and(|picker| picker.id == id) {
                    tracing::info!("nobody chose what to share");
                    state.cancel_screencast_pick();
                }
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            },
        );
    }

    /// Whether there is a shell — to draw a chooser, or to be sent input.
    pub fn shell_is_up(&self) -> bool {
        #[cfg(feature = "wpe")]
        {
            self.shell.is_some()
        }
        #[cfg(not(feature = "wpe"))]
        {
            false
        }
    }

    /// Everything the application could be given a picture of.
    ///
    /// Windows before monitors, and the focused window first: what somebody
    /// means to share is usually what they were just looking at, and the list
    /// is walked from the top.
    fn screencast_sources(&self, types: u32) -> Vec<crate::screencast::Source> {
        let mut sources = Vec::new();
        if types & crate::screencast::SOURCE_WINDOW != 0 {
            let focused = self.views.get(self.focused).filter(|view| view.mapped);
            if let Some(view) = focused {
                sources.push(crate::screencast::Source::Window(view.id));
            }
            for view in self.views.iter() {
                if view.mapped && Some(view.id) != focused.map(|view| view.id) {
                    sources.push(crate::screencast::Source::Window(view.id));
                }
            }
        }
        if types & crate::screencast::SOURCE_MONITOR != 0 {
            // The one being looked at first, for the same reason.
            let active = self
                .active_output
                .as_ref()
                .and_then(|name| self.output_by_name(name));
            if let Some(output) = active.clone() {
                sources.push(crate::screencast::Source::Output(output));
            }
            for output in self.space.outputs() {
                if Some(output) != active.as_ref() {
                    sources.push(crate::screencast::Source::Output(output.clone()));
                }
            }
        }
        sources
    }

    /// Send the chooser, or what is left of it, to the shell.
    fn notify_picker(&mut self) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let id = picker.id;
        let selected = picker.selected as u32;
        let sources = picker
            .sources
            .iter()
            .map(|source| match source {
                crate::screencast::Source::Output(output) => {
                    let properties = output.physical_properties();
                    viewport_ipc::CastSource {
                        kind: "output".to_owned(),
                        label: output.name(),
                        detail: format!("{} {}", properties.make, properties.model)
                            .trim()
                            .to_owned(),
                    }
                }
                crate::screencast::Source::Window(id) => {
                    let view = self.views.get(*id);
                    viewport_ipc::CastSource {
                        kind: "window".to_owned(),
                        label: view.map(|view| view.title()).unwrap_or_default(),
                        detail: view.map(|view| view.app_id()).unwrap_or_default(),
                    }
                }
            })
            .collect();

        let event = Event::ScreencastPick {
            id,
            sources,
            selected,
        };
        self.notify(&event);
    }

    /// Move the highlight.
    pub fn step_screencast_pick(&mut self, delta: isize) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        picker.step(delta);
        self.notify_picker();
    }

    /// Share what is highlighted.
    pub fn confirm_screencast_pick(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        let id = picker.id;
        let Some(source) = picker.sources.into_iter().nth(picker.selected) else {
            let _ = picker.reply.try_send(Err("nothing was chosen".to_owned()));
            self.notify(&Event::ScreencastPickDone { id });
            return;
        };
        let answer = self.begin_cast(source);
        let _ = picker.reply.try_send(answer);
        self.notify(&Event::ScreencastPickDone { id });
        self.restore_focus(picker.restore);
    }

    /// Share nothing, which the application is told is a refusal.
    pub fn cancel_screencast_pick(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        // Dropping the sender would answer too — the other end reads a closed
        // channel as no answer — but saying so keeps the reason in one place.
        let _ = picker.reply.try_send(Err("nothing was chosen".to_owned()));
        self.notify(&Event::ScreencastPickDone { id: picker.id });
        self.restore_focus(picker.restore);
    }

    /// Put the keyboard back where the chooser found it.
    ///
    /// A window that has closed in the meantime is left alone: focus stays
    /// nowhere, which is what it would have been anyway.
    fn restore_focus(&mut self, id: u32) {
        if self.views.get(id).is_some_and(|view| view.mapped) {
            crate::apply::focus_view(self, id);
        }
    }

    /// Start sharing one source, described the way the portal wants it.
    fn begin_cast(
        &mut self,
        source: crate::screencast::Source,
    ) -> Result<crate::screencast::portal::Started, String> {
        let source_type = source.kind();
        self.start_cast(source)
            .map(|(node, size)| crate::screencast::portal::Started {
                node,
                width: size.w,
                height: size.h,
                source_type,
            })
            .map_err(|e| e.to_string())
    }

    /// Whether an output is currently in HDR.
    pub fn hdr_enabled(&self, name: &str) -> bool {
        self.udev
            .as_ref()
            .map(|udev| {
                udev.surfaces()
                    .any(|surface| surface.output.name() == name && surface.hdr)
            })
            .unwrap_or(false)
    }

    /// Whether an output's display would accept HDR at all.
    ///
    /// Read from the connector rather than remembered, because it is a
    /// property of whatever is plugged in — the answer changes when the cable
    /// moves. A shell that offers the toggle on a display that cannot take it
    /// offers a key that does nothing.
    pub fn hdr_capable(&self, name: &str) -> bool {
        self.udev
            .as_ref()
            .and_then(|udev| {
                // By output rather than by surface, because the answer needs
                // the device as well as the connector: a connector handle is
                // device-local, so asking the primary about a monitor on the
                // second card reports a display that does support HDR as one
                // that does not — or answers from an unrelated connector that
                // happens to share the handle.
                let (id, connector) = udev
                    .outputs()
                    .find(|(_, surface)| surface.output.name() == name)
                    .map(|(id, surface)| (id, surface.connector))?;
                let device = udev.devices.get(id.device)?.manager.device();
                Some(crate::hdr::capable(device, connector))
            })
            .unwrap_or(false)
    }

    /// Switch an output into or out of HDR.
    ///
    /// Two properties on the connector, because Smithay's DRM backend has no
    /// notion of either. The client half — a client saying what its content
    /// actually is, and the renderer converting — is already here; without it
    /// this would only make every SDR window look washed out.
    pub fn set_hdr(&mut self, name: &str, enabled: bool) -> anyhow::Result<()> {
        let Some(udev) = self.udev.as_mut() else {
            anyhow::bail!("HDR needs the drm backend");
        };
        let Some((crtc, connector)) = udev
            .outputs()
            .find(|(_, surface)| surface.output.name() == name)
            .map(|(id, surface)| (id, surface.connector))
        else {
            anyhow::bail!("no such output");
        };

        // The card this screen is on. A connector handle means nothing on any
        // other, so asking the primary about a monitor plugged into the second
        // card either finds nothing — and reports a display that does support
        // HDR as one that does not — or finds an unrelated connector with the
        // same handle and turns HDR on for the wrong screen.
        let Some(gpu) = udev.devices.get_mut(crtc.device) else {
            anyhow::bail!("no such gpu");
        };
        let device = gpu.manager.device();
        if !crate::hdr::capable(device, connector) {
            anyhow::bail!("the display does not offer BT.2020 with PQ metadata");
        }
        crate::hdr::set(device, connector, enabled)?;

        if let Some(surface) = udev.surface_mut(crtc) {
            surface.hdr = enabled;
        }
        tracing::info!("{name}: HDR {}", if enabled { "on" } else { "off" });

        // The renderer converts into whatever the output is in, so it has to
        // be told what that now is — otherwise every window is reinterpreted
        // rather than converted, which is the washed-out look this exists to
        // avoid.
        let description = if enabled {
            viewport_vulkan::color::Description {
                primaries: viewport_vulkan::color::Primaries::BT2020,
                transfer: viewport_vulkan::color::TransferFunction::Pq,
                reference_luminance: 203.0,
            }
        } else {
            viewport_vulkan::color::Description::default()
        };
        // Not set here: the renderer has one output description and both
        // monitors draw through it, so setting it when a single display went
        // HDR converted everything on both — an SDR desktop reinterpreted as
        // PQ, which is the washed-out white the other screen showed. The
        // description belongs to whichever output is being drawn, so it is
        // set per frame in `udev::render` from that surface's own state.
        let _ = description;

        // The clients, too. They were told what this output was when they
        // connected and have no way to notice it changed, so a screen switched
        // into HDR goes on being drawn for by every one of them as though it
        // were SDR until it is said out loud.
        self.notify_output_colour(name);
        // And the shell, which draws the HDR badge from this.
        self.notify_output_layout();

        // Everything on screen was drawn for the old colour space.
        self.needs_render = true;
        Ok(())
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
        for surface in udev.surfaces_mut() {
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
            for surface in udev.surfaces_mut() {
                surface.pending = false;
                // Everything that was on screen went with the blanking, and
                // the damage history does not know it. Without this the screen
                // comes back holding whatever last moved and nothing else.
                surface.drm_output.reset_buffers();
            }
            self.needs_render = true;
            return;
        }

        for surface in udev.surfaces_mut() {
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
        let url = self
            .fallback_url
            .clone()
            .unwrap_or_else(|| shipped_asset("fallback.html"));
        tracing::error!(
            "the shell painted nothing within {}ms; loading {url}",
            self.load_timeout_ms
        );
        if let Some(shell) = self.shell.as_ref() {
            // Fire and forget: the load happens on the web thread, so there is
            // no error to catch here. One that fails says so in the log from
            // there.
            shell.load(&url);
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

        for placed in
            crate::watchdog::columns(&ids, (origin.0, origin.1, width as i32, height as i32))
        {
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
                    // No clip and no frame: both describe what the shell drew,
                    // and there is no shell answering.
                    clip: None,
                    frame: None,
                    scale: None,
                    floating: false,
                }),
            );
        }

        // And focus the window that was waiting, because nothing else will.
        // Focus is the shell's to give — `view.focus` over IPC — so a shell
        // that never placed the window never focuses it either, and what the
        // fallback produced without this was a window on screen that no key
        // reached: visible, laid out, and unusable.
        if self.focused == crate::views::NO_VIEW || self.views.get(self.focused).is_none() {
            crate::apply::focus_view(self, id);
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

    /// Tell the idle machinery whether anything is holding it off.
    ///
    /// Dead and unmapped surfaces are dropped first: a client that exits
    /// without releasing its inhibitor would otherwise keep the screen awake
    /// for the rest of the session, and it is in no position to say so.
    pub fn refresh_idle_inhibit(&mut self) {
        use smithay::utils::IsAlive as _;
        self.idle_inhibitors.retain(|surface| surface.alive());
        let inhibited = !self.idle_inhibitors.is_empty();
        self.idle.set_inhibited(inhibited);
        self.idle_notifier_state.set_is_inhibited(inhibited);
    }

    /// One idle tick: lock and blank when their deadlines pass.
    pub fn idle_tick(&mut self) {
        // Every tick, because a client holding one may have died since the
        // last, and nothing else notices.
        self.refresh_idle_inhibit();
        if !self.idle_settings.wanted() {
            return;
        }
        let elapsed = self.idle.since.elapsed();
        let actions = self.idle.tick(&self.idle_settings, elapsed);
        if actions.lock {
            tracing::info!("idle for {}s; locking", elapsed.as_secs());
            self.lock_session();
        }
        if actions.blank {
            tracing::info!("idle for {}s; blanking", elapsed.as_secs());
            // Already flagged by `tick`, so only the screens are left to turn
            // off — `blank_screens` would be a no-op on the flag and is not
            // used here to keep the deadline's bookkeeping in one place.
            self.set_outputs_enabled(false);
        }
    }

    /// Run the configured locker.
    ///
    /// Shared by the idle deadline and the `lock` binding so there is one
    /// answer to what locking means, as in `src/binding.c:614`.
    pub fn lock_session(&mut self) {
        match self.idle_settings.lock_command.clone() {
            Some(command) => crate::input::spawn(&command),
            // Nothing to run. Said rather than silently doing nothing, because
            // a `lock` binding with no `idle.lock_command` looks like it should
            // work — and from the keyboard there is no deadline to blame.
            None => tracing::warn!("lock: no idle.lock_command is set; nothing to run"),
        }
    }

    /// Turn the screens off now.
    ///
    /// Flagged as though the deadline had done it, so the next input brings
    /// them back through the same path. Blanking without that leaves no way to
    /// undo it short of a deadline that has already fired.
    pub fn blank_screens(&mut self) {
        self.idle.force_blank();
        self.set_outputs_enabled(false);
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
                mode: mode.map(
                    |(width, height, refresh)| viewport_ipc::request::ModeRequest {
                        width,
                        height,
                        // Zero means "any rate at this resolution", which is what
                        // a mode string without one asks for.
                        refresh: refresh.unwrap_or(0),
                    },
                ),
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

        // Front to back, which is the order the renderer draws in and the
        // order `Frame::windows` is documented to be in. Smithay's space
        // yields the other way round — bottom of the stack first — so taking
        // it as it comes drew the stack inside out: whatever had just been
        // raised went to the back. Two windows that never overlap look
        // identical either way, which is why a tiling desktop hid this and a
        // floating or maximised window over a tiled one did not.
        let windows: Vec<_> = self
            .space
            .elements()
            .rev()
            .filter_map(|window| {
                let layout = self.space.element_geometry(window)?;
                // Off this output entirely: drawing it would cost a texture
                // bind for something wholly clipped away.
                if !output_geometry.overlaps(layout) {
                    return None;
                }
                let view = window
                    .toplevel()
                    .map(|toplevel| toplevel.wl_surface().clone())
                    .and_then(|surface| self.views.find_by_surface(&surface));
                let overlay_ids: [smithay::backend::renderer::element::Id; 4] = view
                    .map(|view| view.overlay_ids.clone())
                    .unwrap_or_else(|| {
                        std::array::from_fn(|_| smithay::backend::renderer::element::Id::new())
                    });
                let clip = view.and_then(|view| view.clip).map(|clip| {
                    Rectangle::<i32, Logical>::new(
                        (clip.x, clip.y).into(),
                        (clip.width, clip.height).into(),
                    )
                });
                let (location, clip) =
                    crate::render::window_placement(window, layout, output_geometry, clip, scale);

                // The shell's border for this window, where it has said one
                // has to be drawn above whatever is underneath — as four
                // sides around the hole rather than one rectangle over it.
                let overlay = view
                    .and_then(|view| view.frame.map(|frame| (frame, view.box_)))
                    .map(|(frame, hole)| {
                        crate::render::border_sides(frame, hole)
                            .into_iter()
                            .zip(overlay_ids.iter().cloned())
                            .filter_map(|(side, id)| {
                                let local = smithay::utils::Rectangle::<i32, Logical>::new(
                                    (
                                        side.x - output_geometry.loc.x,
                                        side.y - output_geometry.loc.y,
                                    )
                                        .into(),
                                    (side.width, side.height).into(),
                                );
                                // A side of no thickness is a border the shell
                                // did not draw on that edge.
                                (side.width > 0 && side.height > 0)
                                    .then(|| (id, local.to_f64().to_physical(scale).to_i32_round()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                // Where the window's own corner is, as opposed to where its
                // surface starts: the difference is the shadow a client draws
                // outside its geometry, and it is what a thumbnail has to be
                // scaled about.
                let origin = (layout.loc - output_geometry.loc)
                    .to_f64()
                    .to_physical(scale)
                    .to_i32_round();

                Some(crate::render::WindowFrame {
                    window: window.clone(),
                    location,
                    origin,
                    clip,
                    // Both were stored and neither was ever applied: the
                    // overview drew its thumbnails and the compositor painted
                    // full-size windows into them, and a window faded out by
                    // the shell stayed solid.
                    scale: view.map(|view| view.scale).unwrap_or(1.0),
                    opacity: view.map(|view| view.opacity).unwrap_or(1.0),
                    overlay,
                })
            })
            .collect();

        // How many popups are about to be drawn, said when it changes. A menu
        // that is created, configured and then drawn zero times is a
        // different fault from one that is drawn somewhere unhelpful.
        {
            use smithay::desktop::PopupManager;
            use smithay::wayland::seat::WaylandFocus as _;
            let popups: usize = windows
                .iter()
                .filter_map(|frame| frame.window.wl_surface())
                .map(|surface| PopupManager::popups_for_surface(&surface).count())
                .sum();
            // Per output: one monitor drawing a menu and the other not is the
            // ordinary case, and a single counter flapped between them once a
            // frame.
            let seen = self.popups_drawn.entry(output.name()).or_default();
            if popups != *seen {
                *seen = popups;
                tracing::debug!("popup: {popups} being drawn on {}", output.name());
            }
        }

        let cursor = self.cursor_for(output, output_geometry, scale);

        #[cfg(feature = "wpe")]
        let shell = self
            .shell_owned
            .as_ref()
            .map(|(buffer, _)| crate::render::Shell {
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

        // The part of the shell that goes above the windows, in this output's
        // own physical coordinates.
        let visible = smithay::utils::Rectangle::<i32, Logical>::from_size(
            (output_geometry.size.w, output_geometry.size.h).into(),
        );
        let overlay: Vec<_> = self
            .shell_overlays
            .iter()
            .enumerate()
            .filter_map(|(at, rect)| {
                let local = smithay::utils::Rectangle::<i32, Logical>::new(
                    (
                        rect.loc.x - output_geometry.loc.x,
                        rect.loc.y - output_geometry.loc.y,
                    )
                        .into(),
                    rect.size,
                );
                // Nothing of it on this monitor: a notification is drawn on one
                // of them and the others carry on as they were.
                local.intersection(visible)?;
                let id = self.shell_overlay_ids.get(at)?.clone();
                Some((id, local.to_f64().to_physical(scale).to_i32_round()))
            })
            .collect();

        crate::render::Frame {
            layers_above,
            windows,
            layers_below,
            shell,
            overlay,
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

        let status =
            crate::cursor::active_image(self.tablet_cursor_status.as_ref(), &self.cursor_status);

        match status {
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
                match self
                    .cursor_theme
                    .image(shape.name(), scale.ceil() as i32, millis)
                {
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
            XWaylandEvent::Ready {
                x11_socket,
                display_number,
            } => {
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

    /// The same, including outputs that are switched off.
    ///
    /// A disabled output is unmapped from the space — the shell places windows
    /// from the layout, and a monitor that is off has no place in it — so it
    /// cannot be found by looking there. Everything that configures an output
    /// has to use this instead, or turning one back on names a monitor the
    /// compositor insists is gone.
    pub fn any_output_by_name(&self, name: &str) -> Option<Output> {
        if let Some(output) = self.output_by_name(name) {
            return Some(output);
        }
        self.udev.as_ref().and_then(|udev| {
            udev.surfaces()
                .find(|surface| surface.output.name() == name)
                .map(|surface| surface.output.clone())
        })
    }

    /// Announce every mapped window, as a replay.
    ///
    /// This is how a shell that reloaded rebuilds its tree: the windows are not
    /// new, so `replay` is set and the shell restores them into the slots they
    /// left rather than appending them wherever there is room.
    pub fn notify_views(&mut self) {
        // Where each window actually is, not where the next one would go.
        //
        // This sent `output_for_new_view()` — one answer, the active output,
        // for every window in the list. That is the right answer to "where
        // does a new window belong" and no answer at all to "where is this
        // one": a replay across two monitors told the shell that everything
        // was on whichever screen happened to be active, so a shell rebuilding
        // its tree after a reload had every window's output wrong except by
        // luck.
        //
        // A window that is mapped but in no output's region falls back to the
        // same guess as before. That is a window the space has nowhere to put,
        // which is the case the old answer was already the only one for.
        let fallback = self.output_for_new_view();
        let events: Vec<Event> = self
            .views
            .iter()
            .filter(|v| v.mapped)
            .map(|v| {
                let output = self
                    .space
                    .outputs_for_element(&v.window)
                    .first()
                    .map(|o| o.name())
                    .unwrap_or_else(|| fallback.clone());
                Event::ViewAdded(v.added(output, true))
            })
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
            // Checked here for the same reason tiling_mode is, below: this is
            // where the name can be rejected while the file it came from is
            // still in hand. Unchecked, a typo reached the shell, matched none
            // of the models, and left it on the tiling default — while the
            // keymap was built for a layout that does not exist, so the chords
            // belonging to whichever one was meant were simply absent. What
            // that looks like is a config key that was ignored in silence.
            const LAYOUTS: [&str; 3] = ["tiling", "scrolling", "solar"];
            if LAYOUTS.contains(&layout.as_str()) {
                self.config.layout = layout;
            } else {
                tracing::warn!(
                    "unknown layout {layout:?}; expected one of {}",
                    LAYOUTS.join(", ")
                );
            }
        }
        if let Some(logo) = file.logo {
            self.config.logo = logo;
        }
        if let Some(crosses) = file.focus_crosses_outputs {
            self.config.focus_crosses_outputs = crosses;
        }
        if let Some(mode) = file.tiling_mode {
            // Checked here rather than in the shell, because this is where the
            // name can be rejected with the file it came from. An unknown one
            // would otherwise reach the shell, fail to match any arrangement,
            // and leave the tree manual with nothing said.
            const MODES: [&str; 4] = ["manual", "master-stack", "spiral", "bsp"];
            if MODES.contains(&mode.as_str()) {
                self.config.tiling_mode = Some(mode);
            } else {
                tracing::warn!(
                    "unknown tiling_mode {mode:?}; expected one of {}",
                    MODES.join(", ")
                );
            }
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
            // Running applications change on the portal's signal; without this
            // a reload would move the setting and nothing on screen with it.
            self.appearance.set_dark(dark);
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
        let layout = self.config.layout.clone();

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
            None => bindings.extend(crate::binding::defaults(&terminal, &menu, &layout)),
        }
        self.bindings = bindings;
    }

    /// How many empty ticks before the barrier clock stops. A second at sixty
    /// hertz, and a commit starts it again.
    const QUIET: u32 = 60;

    /// Let go of everything a client is waiting on for this frame.
    ///
    /// Two protocols block a commit until the compositor says so: wp-fifo,
    /// where a client asks to be paced by the display, and wp-commit-timing,
    /// where it asks for a commit to land at a particular time. Both are the
    /// compositor's to release, and a client whose barrier is never signalled
    /// does not simply lose the feature — it never paints again.
    ///
    /// Called where the frame callbacks are sent, which is the moment the
    /// frame this surface is part of has been handed to the display.
    pub fn release_frame_barriers(&mut self, output: &Output, frame_target: std::time::Duration) {
        let _ = self.released_frame_barriers(output, frame_target);
    }

    /// The same, reporting whether anything was actually let go.
    ///
    /// The tick needs to know: a round that releases nothing is a round that
    /// did not need to happen, and enough of those in a row means the clock
    /// can stop.
    pub fn released_frame_barriers(
        &mut self,
        output: &Output,
        frame_target: std::time::Duration,
    ) -> bool {
        self.released_barriers(output, frame_target, false)
    }

    /// Let go of commit-timing deadlines that are already past, and nothing
    /// else.
    ///
    /// For a screen this round is otherwise skipping because its own vblank is
    /// doing the releasing. A fifo barrier there is that vblank's to take —
    /// taking it here is what paced clients off this timer instead of off the
    /// screen. A deadline that has *already passed* is different: it is not
    /// pacing anything, it is only holding a commit.
    ///
    /// It holds it because Smithay blocks every commit carrying a deadline
    /// whether or not the deadline has arrived — unlike its fifo hook, which
    /// skips a barrier that is already signalled. So a commit aimed at a
    /// moment that has been and gone waits for whatever runs next, and on a
    /// screen with no frame coming that is this and only this.
    pub fn release_overdue_timers(&mut self, output: &Output) -> bool {
        let at = self.start_time.elapsed();
        self.released_barriers(output, at, true)
    }

    fn released_barriers(
        &mut self,
        output: &Output,
        frame_target: std::time::Duration,
        overdue_only: bool,
    ) -> bool {
        use smithay::desktop::utils::with_surfaces_surface_tree;
        use smithay::utils::Time;
        use smithay::wayland::commit_timing::CommitTimerBarrierStateUserData;
        use smithay::wayland::fifo::FifoBarrierCachedState;

        // The clock the *client* set its deadline on, which is CLOCK_MONOTONIC
        // and not this compositor's uptime. `frame_target` is time since the
        // compositor started — smaller than the real clock by however long the
        // machine has been up — so using it means every deadline is in the
        // future for ever and every timed commit blocks until the client gives
        // up. That is a client frozen on its first frame, and it looks exactly
        // like the fifo barrier not being signalled.
        let _ = frame_target;
        let now = smithay::reexports::rustix::time::clock_gettime(
            smithay::reexports::rustix::time::ClockId::Monotonic,
        );
        // The deadline to compare against is the frame this round is about to
        // draw, which is the *next* refresh — not this instant.
        //
        // A commit-timing deadline says "do not show this before T". The frame
        // being built now is the one that will be presented at the next
        // vblank, so what belongs in it is everything due by then. Comparing
        // against the present moment instead holds a commit aimed at the next
        // vblank until that vblank has already happened, so it misses the
        // frame it was aimed at and goes in the one after.
        //
        // Mesa aims exactly there — one refresh ahead — on every frame, so
        // every frame was arriving a frame late, and any jitter in when this
        // round ran turned "late" into "a whole refresh late". That is the
        // client sitting at five sixths of the rate: with commit-timing off
        // and fifo left on, the same client goes from 204.1fps to 239.2 of a
        // possible 239.76.
        //
        // Except when only the overdue are wanted: then the moment is now, so
        // a deadline aimed at the frame after this one is left for the vblank
        // that will actually show it.
        let refresh = if overdue_only {
            std::time::Duration::ZERO
        } else {
            self.frame_interval()
        };
        let target: Time<smithay::utils::Monotonic> =
            (std::time::Duration::new(now.tv_sec as u64, now.tv_nsec as u32) + refresh).into();
        let released = std::cell::Cell::new(false);
        // Counted rather than just flagged: `released` answers "was this round
        // worth running", and what the pacing question needs is how many.
        let signalled = std::cell::Cell::new(0u32);
        let woken: std::cell::RefCell<Vec<smithay::reexports::wayland_server::Client>> =
            std::cell::RefCell::new(Vec::new());

        let release = |surface: &WlSurface, states: &smithay::wayland::compositor::SurfaceData| {
            let wake = |signalled: bool| {
                if !signalled {
                    return;
                }
                if let Some(client) = surface.client() {
                    let mut woken = woken.borrow_mut();
                    if !woken.iter().any(|c| c.id() == client.id()) {
                        woken.push(client);
                    }
                }
            };
            if let Some(mut timer) = states
                .data_map
                .get::<CommitTimerBarrierStateUserData>()
                .map(|timer| timer.lock().unwrap())
            {
                if timer.signal_until(target) {
                    tracing::trace!("commit-timing: a deadline reached");
                    released.set(true);
                    wake(true);
                }
            }
            // The current half, taken out. This is what `wayland::fifo`
            // documents and what anvil does, and both halves of that matter.
            //
            // Taken, because a barrier left in place is found again on the next
            // round, already signalled, and reported as nothing released —
            // which is what `QUIET` counts, and enough of those stop the clock
            // under a client that is still waiting.
            //
            // Current only, because pending is where the pre-commit hook looks
            // for the barrier to hand the *next* commit, and it skips blocking
            // outright if what it finds is already signalled
            // (`wayland/fifo/mod.rs:257`). Signalling pending is therefore not
            // a belt-and-braces release: it is switching the pacing off. The
            // barrier a blocked commit is waiting on is not lost by leaving
            // pending alone — Smithay carries it in the transaction and puts it
            // in the current half when that commit applies, which is the round
            // this signals it in.
            // Left alone entirely when only the overdue are wanted: this
            // screen has a vblank coming, and that vblank is what a fifo
            // barrier means. Taking it here would pace the client off this
            // timer rather than off the screen, which is the drift measured at
            // five sixths of the refresh rate.
            let barrier = if overdue_only {
                None
            } else {
                states
                    .cached_state
                    .get::<FifoBarrierCachedState>()
                    .current()
                    .barrier
                    .take()
            };
            if let Some(barrier) = barrier {
                barrier.signal();
                tracing::trace!("fifo: a barrier released");
                released.set(true);
                signalled.set(signalled.get() + 1);
                wake(true);
            }
        };

        // Only the windows on this output. Walking every window once per
        // output did the same work twice on a two-monitor desktop and made a
        // release on one screen look like a reason to draw the other.
        for window in self.space.elements_for_output(output) {
            window.with_surfaces(&release);
        }
        for layer in smithay::desktop::layer_map_for_output(output).layers() {
            layer.with_surfaces(&release);
        }
        for lock in self.lock_surfaces.values() {
            with_surfaces_surface_tree(lock.wl_surface(), &release);
        }
        // After the walks, so the closure's borrows are done with.
        let signalled = signalled.get();
        if signalled > 0 {
            if let Some(log) = self.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
                log.barriers += signalled;
            }
        }
        // The part that makes any of it work. Signalling a barrier only sets a
        // flag; the commit it was blocking sits in a queue that nothing looks
        // at again until the compositor says a blocker cleared. Without this
        // the client commits for ever and the compositor applies none of them,
        // which from the outside is a window frozen on its first frame while
        // the client is busy and healthy.
        let woken = woken.into_inner();
        if !woken.is_empty() {
            let dh = self.display_handle.clone();
            for client in woken {
                if let Some(data) = client.get_data::<crate::state::ClientState>() {
                    data.compositor_state.blocker_cleared(self, &dh);
                }
            }
        }
        released.get()
    }

    /// Whether anything is waiting on a barrier, as far as can be told.
    ///
    /// A fifo barrier sits in the surface's current state from the commit that
    /// set it until the compositor takes it, so it can be seen directly. A
    /// commit timer cannot: Smithay keeps its deadlines private and offers no
    /// way to ask whether any are left. So a surface that has ever used one
    /// counts as waiting, and `arm_barrier_tick` stops re-arming after a
    /// stretch of ticks that release nothing — the next commit starts the
    /// clock again, which is the only moment a new deadline can appear.
    pub fn barriers_outstanding(&self) -> bool {
        use smithay::wayland::commit_timing::CommitTimerBarrierStateUserData;
        use smithay::wayland::fifo::{FifoBarrierCachedState, FifoCachedState};

        let mut waiting = false;
        {
            let mut look =
                |_surface: &WlSurface, states: &smithay::wayland::compositor::SurfaceData| {
                    if waiting {
                        return;
                    }
                    // Not "is a barrier sitting here" — that misses the case
                    // this whole tick exists for. A commit blocked on a barrier
                    // has had that barrier taken out of the surface state by
                    // the pre-commit hook and handed to the blocker, so the
                    // surface looks empty at exactly the moment the client is
                    // stuck. What it does not hide is that the client asked
                    // for fifo at all, which is in `FifoCachedState`.
                    //
                    // So: a surface that uses either protocol keeps the clock
                    // running. A fifo client wants a frame every refresh
                    // anyway, and the frame is what carries the callback and
                    // the presentation feedback it is waiting on.
                    let mut fifo_request = states.cached_state.get::<FifoCachedState>();
                    let asks_for_fifo = {
                        let pending = *fifo_request.pending();
                        let current = *fifo_request.current();
                        pending.set_barrier
                            || pending.wait_barrier
                            || current.set_barrier
                            || current.wait_barrier
                    };
                    let mut fifo = states.cached_state.get::<FifoBarrierCachedState>();
                    if asks_for_fifo
                        || fifo.current().barrier.is_some()
                        || fifo.pending().barrier.is_some()
                        || states
                            .data_map
                            .get::<CommitTimerBarrierStateUserData>()
                            .is_some()
                    {
                        waiting = true;
                    }
                };
            for window in self.space.elements() {
                window.with_surfaces(&mut look);
            }
        }
        waiting
    }

    /// Keep a clock running while a client is blocked on a barrier.
    ///
    /// This is the half that was missing when these two protocols were first
    /// advertised and then withdrawn. The compositor draws when there is
    /// damage; a blocked commit produces none, so the frame that would have
    /// released the barrier never happens and the client waits for a
    /// compositor that is waiting for the client. Six hundred lines of
    /// "nothing to draw" and a terminal frozen on its first frame.
    ///
    /// So while anything is outstanding, a timer runs at roughly the refresh
    /// interval, signals what is due, and asks for a frame. Once nothing is
    /// waiting the timer stops, and an idle desktop goes back to drawing
    /// nothing at all.
    pub fn arm_barrier_tick(&mut self) {
        if self.barrier_tick {
            return;
        }
        if !self.barriers_outstanding() {
            return;
        }
        let interval = self.frame_interval();

        // On a timerfd, for the same reason the frame clock is: this tick is
        // the *only* thing that can free a client blocked on a barrier when
        // nothing else is happening. A blocked commit makes no damage, so
        // there is no frame, so there is no vblank, so `on_vblank` — the other
        // half of the release — never runs either. Under the web engine GLib
        // owns the blocking poll and cannot see a calloop timer, so this tick
        // used to arrive only when a mouse or another window woke the loop for
        // unrelated reasons. That is a terminal on an empty workspace showing
        // nothing of what is typed into it: rio paints through Mesa, Mesa
        // paces itself with `wp_fifo_v1`, and every one of its commits waits
        // on a barrier this tick was supposed to lift.
        if self.barrier_timer.is_none() {
            self.barrier_timer = self.create_tick("barrier tick", Self::release_barriers);
        }
        if Self::arm_tick("barrier tick", self.barrier_timer.as_ref(), interval) {
            self.barrier_tick = true;
            return;
        }

        self.barrier_tick = true;
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(interval);
        if let Err(e) = self.loop_handle.insert_source(timer, move |_, _, state| {
            state.release_barriers();
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        }) {
            tracing::warn!("arming the barrier tick: {e}");
            self.barrier_tick = false;
        }
    }

    /// One turn of the barrier tick: let go of whatever is due.
    fn release_barriers(&mut self) {
        self.barrier_tick = false;
        if let Some(log) = self.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
            log.barrier_ticks += 1;
        }

        // Not while the vblank is doing it.
        //
        // A fifo barrier says "the frame this commit made has been shown", so
        // the moment to signal it is the vblank that showed it. This tick is
        // for a desk where no frame is being submitted and so no vblank is
        // coming — a client blocked on a barrier makes no damage, which makes
        // no frame, which makes no vblank to lift it.
        //
        // Left running alongside the vblank it does not add safety, it takes
        // the job over. It fires part way through the frame period and takes
        // the barrier before the frame it belongs to has been presented, so
        // the vblank arrives to an empty queue and does nothing — measured at
        // 60Hz: 50 barriers a second, every one of them released here and none
        // at the vblank, with ten vblanks a second finding nothing to do.
        //
        // The client is then paced by this timer rather than by the screen,
        // and this timer is armed after its own work, so it is always slower.
        // That is the whole of a client sitting at five sixths of the refresh
        // rate at every rate tried — 50.4 of 60, 101.8 of 120, 203.4 of 240 —
        // while the compositor flipped on every single vblank at under 2% of
        // a core.
        //
        // Still re-armed below, so it takes over within two frames if the
        // chain really does stop.
        //
        // Asked once per screen rather than once for the device. It used to be
        // one stamp — "has *anything* flipped lately" — and the release it
        // defers to is one screen's windows, so a second monitor animating at
        // the refresh rate kept that stamp fresh and silenced this tick for a
        // screen it never visited. Measured on a two-screen desk: 238 turns a
        // second, every one of them deferred, and every one of those deferrals
        // made on behalf of a screen that had not flipped.
        //
        // What waits behind it is worse than a late fifo barrier. Smithay
        // blocks *every* commit carrying a commit-timing deadline, including
        // one already in the past — unlike its fifo hook, which skips a
        // barrier that is already signalled — so this pass is the only thing
        // that lets such a commit through. Deferring it on another screen's
        // behalf is how a terminal ends up at seven frames a second on a 240Hz
        // display with the compositor idle.
        let interval = self.frame_interval();
        let at = self.start_time.elapsed();
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let mut released = false;
        let mut walked = 0usize;
        for output in &outputs {
            // This screen's own last flip, not the newest anywhere. A screen
            // whose vblank is doing the releasing does not need this pass.
            let own_vblank = self.udev.as_ref().is_some_and(|udev| {
                udev.last_vblank_by_output
                    .get(&output.name())
                    .is_some_and(|at| at.elapsed() < interval * 2)
            });
            if own_vblank {
                // Its vblank has the fifo barriers. What that vblank will not
                // do is let go of a deadline that has already passed, because
                // it only signals up to the frame it is about to show — and a
                // commit held on a stale deadline is not waiting for a frame,
                // it is just waiting.
                if self.release_overdue_timers(output) {
                    released = true;
                    self.mark_output_dirty(output);
                }
                continue;
            }
            walked += 1;
            // Counted before the call, so what it adds to `barriers` can be
            // told apart afterwards: this is the tick's share of the releases,
            // and under a client painting flat out it should be nearly none.
            let before = self
                .udev
                .as_ref()
                .and_then(|udev| udev.frame_log.as_ref())
                .map(|log| log.barriers);
            if self.released_frame_barriers(output, at) {
                released = true;
                self.mark_output_dirty(output);
            }
            if let Some(before) = before {
                if let Some(log) = self.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
                    log.barriers_at_tick += log.barriers.saturating_sub(before);
                }
            }
        }
        // Every screen was flipping on its own, so this turn had nothing to
        // do. The same early exit as before, reached per-screen rather than
        // for the device — and `starved` should now stay at zero, because a
        // screen that has not flipped is one this walked.
        if walked == 0 {
            let starved = self.udev.as_ref().is_some_and(|udev| {
                self.space.outputs().any(|output| {
                    udev.last_vblank_by_output
                        .get(&output.name())
                        .is_none_or(|at| at.elapsed() >= interval * 2)
                })
            });
            if let Some(log) = self.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
                log.barrier_ticks_deferred += 1;
                if starved {
                    log.barrier_ticks_starved += 1;
                }
            }
            self.arm_barrier_tick();
            return;
        }
        // Only when something was let go. Releasing a barrier applies the
        // commit it was blocking, and an applied commit is damage, and damage
        // is what asks for a frame — so the frame arrives without being
        // demanded here. Asking anyway drew every output at the refresh rate
        // for as long as one client used the protocol, which on a second
        // monitor with nothing on it is pure heat.
        if released {
            self.barrier_quiet = 0;
        } else {
            self.barrier_quiet = self.barrier_quiet.saturating_add(1);
        }
        // A tick that has released nothing for a second is a tick nobody
        // needs: the deadlines that could not be seen have all passed, or
        // there were none. A commit is the only thing that can make a new one,
        // and a commit arms this again.
        //
        // Unless something is still waiting. `QUIET` is a backstop for
        // commit-timing, whose deadlines Smithay keeps private so an empty
        // round cannot be told from a finished one — but fifo *can* be seen,
        // and a fifo client's blocked commit never reaches `commit()` to arm
        // this again. Letting the count stop the clock under one is how a
        // terminal ends up waiting on a compositor that has stopped looking.
        if self.barrier_quiet < Self::QUIET || self.barriers_outstanding() {
            self.arm_barrier_tick();
        }
    }

    /// Replace the list of shell rectangles that float above the windows.
    ///
    /// Ids are kept by position and only minted when the list grows, because a
    /// render element with a new id every frame tells the damage tracker that
    /// everything changed.
    pub fn set_shell_overlays(&mut self, rects: Vec<smithay::utils::Rectangle<i32, Logical>>) {
        if self.shell_overlays == rects {
            return;
        }
        while self.shell_overlay_ids.len() < rects.len() {
            self.shell_overlay_ids
                .push(smithay::backend::renderer::element::Id::new());
        }
        self.shell_overlays = rects;
        // The stack changed without anything committing, and a desktop nobody
        // is touching produces no damage of its own — so without this the
        // notification appears on the next frame something else happens to
        // cause, which on an idle desktop is none.
        self.needs_render = true;
    }

    /// Ask for a frame on whichever screens show this surface.
    ///
    /// A client painting at its own rate is the common case, and marking the
    /// whole desktop for it means the other monitor attempts a frame per
    /// commit and finds nothing — two thousand of them in five seconds, for a
    /// cube on the first screen. Falls back to everything when the surface is
    /// not a window this compositor has placed: a layer surface, a popup, the
    /// lock screen, or a window between mapping and being given a rectangle.
    pub fn mark_dirty_for_surface(&mut self, surface: &WlSurface) {
        let mut root = surface.clone();
        while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
            root = parent;
        }
        let outputs = self
            .views
            .find_by_surface(&root)
            .map(|view| self.space.outputs_for_element(&view.window))
            .unwrap_or_default();
        if outputs.is_empty() {
            self.needs_render = true;
            return;
        }
        for output in outputs {
            self.mark_output_dirty(&output);
        }
    }

    /// Ask for a frame on one output rather than all of them.
    ///
    /// A pacing barrier belongs to a window, a window is on a screen, and the
    /// other screen has no reason to be redrawn for it. Falls back to marking
    /// everything if the output has no CRTC here, which is the nested backend
    /// and the moment between a monitor arriving and being brought up.
    pub fn mark_output_dirty(&mut self, output: &Output) {
        self.arm_frame_clock();
        let crtc = self.udev.as_ref().and_then(|udev| {
            udev.outputs()
                .find(|(_, surface)| &surface.output == output)
                .map(|(id, _)| id)
        });
        match crtc {
            Some(crtc) => {
                self.dirty_outputs.insert(crtc);
            }
            None => self.needs_render = true,
        }
    }

    /// How long one frame lasts on the fastest output, near enough.
    ///
    /// Near enough because this paces a fallback clock rather than the display
    /// itself: a barrier released a millisecond late is a frame late at worst,
    /// and the alternative is no frame ever.
    pub fn frame_interval(&self) -> std::time::Duration {
        self.space
            .outputs()
            .filter_map(|output| output.current_mode())
            .map(|mode| mode.refresh.max(1) as u64)
            .max()
            .map(|refresh| std::time::Duration::from_nanos(1_000_000_000_000 / refresh))
            .unwrap_or_else(|| std::time::Duration::from_millis(16))
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
                    // The shell owns this — it tracks the pointer and keyboard
                    // focus, and tells the compositor. Reporting it back is
                    // what lets anything else ask which screen the user is on:
                    // a screenshot tool otherwise has to guess, and guessing
                    // over two monitors means capturing both.
                    active: self.active_output.as_deref() == Some(output.name().as_str()),
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
                    hdr: self.hdr_enabled(&output.name()),
                    hdr_capable: self.hdr_capable(&output.name()),
                    scale: output.current_scale().fractional_scale(),
                    transform: crate::apply::from_smithay_transform(output.current_transform()),
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

    /// Which view a toplevel object belongs to.
    pub fn view_for_toplevel(
        &self,
        toplevel: &smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel,
    ) -> Option<u32> {
        use smithay::reexports::wayland_server::Resource as _;
        let id = toplevel.id();
        self.views
            .iter()
            .find(|view| {
                view.window
                    .toplevel()
                    .is_some_and(|t| t.xdg_toplevel().id() == id)
            })
            .map(|view| view.id)
    }

    /// An output has changed shape: a new mode, a new scale, or a rotation.
    ///
    /// Two things stop describing the screen the moment that happens, and
    /// neither notices on its own.
    ///
    /// The layer map holds the output's shape from when it was last arranged,
    /// along with everything reserved against it — a bar's exclusive zone, and
    /// the area left over for windows. Rotating a monitor without re-arranging
    /// it left the usable area landscape on a portrait screen, so the shell was
    /// told the output was 1440x2560 and that windows could use 2560x1440 of
    /// it.
    ///
    /// And the damage history describes a screen that no longer exists. The
    /// framebuffers are reused between frames and only the damaged part of one
    /// is redrawn; a rotation changes every pixel while leaving the buffers the
    /// same size, so whatever the new frame did not report as damaged stayed on
    /// screen — the old landscape desktop with the new portrait one drawn over
    /// part of it, both at once. The same reset the compositor already does
    /// when it comes back from a VT switch, and for the same reason.
    ///
    /// Called from both paths that can reshape an output: the shell's
    /// `output.configure`, and wlr-output-management, which is what `wlr-randr`
    /// speaks. Putting it in one of them and not the other is how this went out
    /// fixed for a tool nobody was using and unfixed for the one being tested.
    pub fn output_reshaped(&mut self, output: &Output) {
        if let Some(loc) = self.space.output_geometry(output).map(|g| g.loc) {
            self.map_output_at(output, loc);
        }
        smithay::desktop::layer_map_for_output(output).arrange();

        if let Some(udev) = self.udev.as_mut() {
            for surface in udev.surfaces_mut() {
                if surface.output == *output {
                    surface.drm_output.reset_buffers();
                    // `pending` is not cleared here, though the VT-switch path
                    // this was taken from does clear it. There, every flip died
                    // with the session; here one is very likely in flight, and
                    // forgetting it means the compositor stops waiting for the
                    // vblank that would tell it the flip landed. What that
                    // produced was a black screen and half a million lines of
                    // "nothing to draw": rendering as fast as the loop would go
                    // and committing nothing.
                }
            }
        }
        self.needs_render = true;
        self.notify_output_layout();
        tracing::info!(
            "{}: reshaped to {:?}",
            output.name(),
            output.current_transform()
        );
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

    /// Invite the surfaces on an output to draw their next frame.
    ///
    /// Split out of the render pass because a frame callback is not a thing
    /// that happens *because* the compositor drew. It is the compositor
    /// saying "now would be a good time", and a client that paints only when
    /// invited has no other way to hear it.
    pub fn send_frame_callbacks(&mut self, output: &Output, at: std::time::Duration) {
        // Half a frame, not a whole one.
        //
        // Smithay drops an invitation unless more than `throttle` has passed
        // since the last one, strictly greater. Set to the refresh period
        // exactly, that is a knife edge laid on top of a clock that jitters:
        // an invitation arriving a microsecond early is not held back, it is
        // thrown away, and the client waits an entire further frame.
        //
        // What that cost was a constant fraction rather than a constant
        // amount, which is what made it so hard to read as a timing bug. A
        // client that drew on every invitation got 50.3fps of 60, 101.8 of
        // 120, and 203.4 of 239.76 — 85% at every rate, while the compositor
        // sat at 3.6% of a core and flipped on every single vblank. It was
        // never short of time. It was being told to draw five times out of
        // six.
        //
        // Half a period leaves the throttle doing its actual job — an
        // occluded surface still cannot be invited faster than twice a frame —
        // without standing exactly where the jitter falls.
        let throttle = Some(self.frame_interval() / 2);

        // Who was actually waiting to be told. Counted before the send,
        // because the send is what empties the queue. See `FrameLog::wanted`.
        if self
            .udev
            .as_ref()
            .and_then(|udev| udev.frame_log.as_ref())
            .is_some()
        {
            use smithay::wayland::compositor::SurfaceAttributes;
            let mut waiting = 0u32;
            for window in self.space.elements() {
                let mut asked = false;
                window.with_surfaces(|_, states| {
                    let queued = states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .frame_callbacks
                        .len();
                    asked |= queued > 0;
                });
                if asked {
                    waiting += 1;
                }
            }
            if let Some(log) = self.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
                log.wanted += waiting;
            }
        }
        for window in self.space.elements() {
            window.send_frame(output, at, throttle, |_, _| Some(output.clone()));
        }
        for layer in smithay::desktop::layer_map_for_output(output).layers() {
            layer.send_frame(output, at, throttle, |_, _| Some(output.clone()));
        }
        for lock in self.lock_surfaces.values() {
            smithay::desktop::utils::send_frames_surface_tree(
                lock.wl_surface(),
                output,
                at,
                throttle,
                |_, _| Some(output.clone()),
            );
        }
    }

    /// Keep inviting clients to draw for as long as any of them is drawing.
    ///
    /// Frame callbacks used to go out only at the end of a render pass, which
    /// worked by accident: the compositor rendered thousands of times a second
    /// whether or not anything had changed, so every client was invited
    /// constantly. Once renders were held to actual damage that engine went
    /// away, and with it every invitation — a client waiting on a callback to
    /// paint never painted, so it never made damage, so no render happened and
    /// no callback went out. The desktop froze solid and came back only on
    /// input, which forced a frame by another route.
    ///
    /// So the invitations get their own clock. It ticks at the refresh rate
    /// while clients are committing and stops when they stop, which is the
    /// difference between a frame clock and the busy loop it replaces.
    pub fn arm_frame_clock(&mut self) {
        // Recorded even when the clock is already running, so that the tick
        // this request lands behind is not the last one. See `frame_pending`.
        self.frame_pending = true;
        if self.frame_clock {
            return;
        }
        let interval = self.frame_interval();

        if self.frame_timer.is_none() {
            self.frame_timer = self.create_tick("frame clock", Self::frame_tick);
        }
        if Self::arm_tick("frame clock", self.frame_timer.as_ref(), interval) {
            self.frame_clock = true;
            self.frame_clock_at = Some(std::time::Instant::now() + interval);
            return;
        }

        // No timerfd. calloop's own timer still works whenever calloop is the
        // one waiting, which is every backend except the web engine's, so it
        // is worth having rather than dropping the tick entirely.
        self.frame_clock = true;
        self.frame_clock_at = Some(std::time::Instant::now() + interval);
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(interval);
        if let Err(e) = self.loop_handle.insert_source(timer, move |_, _, state| {
            state.frame_tick();
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        }) {
            tracing::warn!("frame clock: {e}");
            self.frame_clock = false;
            self.frame_clock_at = None;
        }
    }

    /// Create a timerfd, put it in the loop, and say what a tick does.
    ///
    /// Returns a second handle on the same timer: the source owns the fd it
    /// watches, and arming happens from outside the source.
    ///
    /// A plain `fn` rather than a closure so that the tick body stays a named
    /// method — these run a frame apart from everything else and are easier to
    /// find when they are not anonymous.
    fn create_tick(
        &mut self,
        what: &'static str,
        run: fn(&mut Self),
    ) -> Option<std::os::fd::OwnedFd> {
        use smithay::reexports::rustix::time::{timerfd_create, TimerfdClockId, TimerfdFlags};

        let fd = match timerfd_create(
            TimerfdClockId::Monotonic,
            TimerfdFlags::NONBLOCK | TimerfdFlags::CLOEXEC,
        ) {
            Ok(fd) => fd,
            Err(e) => {
                tracing::warn!("{what}: no timerfd ({e}), falling back to a loop timer");
                return None;
            }
        };
        let watched = match fd.try_clone() {
            Ok(watched) => watched,
            Err(e) => {
                tracing::warn!("{what}: could not dup the timer ({e})");
                return None;
            }
        };

        if let Err(e) = self.loop_handle.insert_source(
            Generic::new(watched, Interest::READ, Mode::Level),
            move |_, fd, state: &mut Self| {
                // Drained, or a level-triggered source reports the same
                // expiry for ever and the loop never sleeps again.
                let mut buf = [0u8; 8];
                let _ = smithay::reexports::rustix::io::read(&*fd, &mut buf[..]);
                run(state);
                Ok(PostAction::Continue)
            },
        ) {
            tracing::warn!("{what}: could not watch the timer ({e})");
            return None;
        }

        Some(fd)
    }

    /// Set a one-shot timerfd `interval` from now. False if there was none to
    /// set, or the kernel refused it.
    ///
    /// One shot rather than repeating: a repeating timer would keep waking a
    /// desktop that has settled, which is the cost these clocks exist to
    /// avoid. Each tick arms the next itself for as long as there is a reason.
    fn arm_tick(
        what: &'static str,
        fd: Option<&std::os::fd::OwnedFd>,
        interval: std::time::Duration,
    ) -> bool {
        use smithay::reexports::rustix::time::{
            timerfd_settime, Itimerspec, TimerfdTimerFlags, Timespec,
        };

        let Some(fd) = fd else {
            return false;
        };
        let spec = Itimerspec {
            it_interval: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: Timespec {
                tv_sec: interval.as_secs() as _,
                tv_nsec: interval.subsec_nanos() as _,
            },
        };
        match timerfd_settime(fd, TimerfdTimerFlags::empty(), &spec) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!("{what}: could not arm the timer: {e}");
                false
            }
        }
    }

    /// One turn of the frame clock: invite, draw, send.
    fn frame_tick(&mut self) {
        self.frame_clock = false;
        self.frame_clock_at = None;
        let asked = std::mem::take(&mut self.frame_pending);

        let at = self.start_time.elapsed();
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();

        // Only when no vblank is doing it.
        //
        // `on_vblank` invites the clients on an output the moment its frame
        // reaches the screen, which is the right moment and the right rate.
        // This clock exists for when that is not happening at all — nested,
        // headless, or a desktop so still that nothing has been submitted and
        // so no vblank is coming. Sending from both is not redundancy: the
        // client is asked twice per frame period and paints twice, and the
        // compositor shows one of the two. A 60Hz screen measured a client at
        // 120fps with half of its work discarded before anyone saw it.
        //
        // Two frame periods of slack, so this takes over promptly when the
        // chain really has stopped without racing it when it has not.
        let vblank_driven = self
            .udev
            .as_ref()
            .and_then(|udev| udev.last_vblank)
            .is_some_and(|at| at.elapsed() < self.frame_interval() * 2);
        if !vblank_driven {
            for output in &outputs {
                self.send_frame_callbacks(output, at);
            }
        }
        // Counted before the render, because this is the moment the clock is
        // about to do a vblank's job: every render driven from here is a
        // flip-vblank-flip chain that had stopped and is being restarted, and
        // this clock is slower than the screen. See `FrameLog`.
        if let Some(log) = self.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
            log.restarts += 1;
        }
        // One render for everything that happened since the last tick.
        self.render_if_needed();
        let _ = self.display_handle.flush_clients();

        // Anything still owed a frame keeps the clock going; an empty desk
        // lets it stop. `asked` is what covers the surface whose invitation
        // this tick was too early to send — see `frame_pending`. Cleared
        // straight after, because the arming below *is* that follow-up and
        // treating it as a fresh request would leave the clock running for
        // ever.
        if asked || self.needs_render || !self.dirty_outputs.is_empty() {
            self.arm_frame_clock();
            self.frame_pending = false;
        }
    }

    /// Draw any output that has something new to show.
    ///
    /// Called from the outer loop rather than from wherever the change
    /// happened, so a commit that touches five subsurfaces costs one frame
    /// instead of five.
    pub fn render_if_needed(&mut self) {
        let all = std::mem::take(&mut self.needs_render);
        let some = std::mem::take(&mut self.dirty_outputs);
        if !all && some.is_empty() {
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
            .map(|udev| {
                udev.ids()
                    .into_iter()
                    .filter(|id| all || some.contains(id))
                    .collect()
            })
            .unwrap_or_default();
        for crtc in crtcs {
            self.render(crtc);
        }
    }

    /// Send every toplevel the configure it has pending.
    ///
    /// Cheap to call on all of them: `send_pending_configure` is a no-op for a
    /// window whose pending state matches what it was last told.
    pub(crate) fn send_pending_configures(&self) {
        for window in self.space.elements() {
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_pending_configure();
            }
        }
    }

    /// Tell the clients which one of them has focus. `NO_VIEW` means none does.
    ///
    /// Two separate things carry focus and only one of them is obvious. The
    /// keyboard focus decides where keys go. The toplevel's `activated` state
    /// decides what the window *looks* like — a toolkit greys its title bar,
    /// dims its selection and stops blinking its cursor without it — and it
    /// only reaches the client on a configure.
    ///
    /// Smithay sets that state when a window is raised with `activate`, but
    /// only into the pending configure, and a configure nobody sends is a
    /// client that never finds out. Tiling hid it: focus there arrives with a
    /// layout change, and the resize that comes with it flushes the pending
    /// state along the way. A floating window is focused where it stands — no
    /// move, no resize, nothing else to send — so it took the keys and stayed
    /// drawn as though it had not: focused and grey.
    pub fn activate_view(&mut self, id: u32) {
        let focused = self.views.get(id).map(|view| view.window.clone());
        // Every window, not only the two that changed. Anything else leaves a
        // window that was activated by some other path still believing it.
        for window in self.space.elements() {
            let active = focused.as_ref() == Some(window);
            window.set_activated(active);
        }
        self.send_pending_configures();
    }

    /// Put the stack back the way the desktop is meant to look: floating
    /// windows above tiled ones.
    ///
    /// The shell owns layout and the compositor owns the stack, so this is the
    /// one stacking rule the compositor keeps for itself. A floating window is
    /// a dialog, a palette or a picture-in-picture that was deliberately put in
    /// front of the layout; behind a tiled window it is not merely hard to see
    /// but unreachable, because `Space` is what a click is tested against as
    /// well as what the renderer draws from.
    ///
    /// Focus does not enter into it. Focusing a tiled window raises that window,
    /// and without this the float it was covering goes under — so this runs
    /// after every raise rather than only after the ones that look risky.
    /// Relative order among the floats is kept: they are re-raised bottom to
    /// top, so the one in front stays in front.
    pub fn restack(&mut self) {
        let floating: Vec<smithay::desktop::Window> = self
            .space
            .elements()
            .filter(|window| {
                self.views
                    .iter()
                    .any(|view| view.floating && view.window == **window)
            })
            .cloned()
            .collect();
        for window in floating {
            self.space.raise_element(&window, false);
        }

        // Above even those: an X11 menu or tooltip, which places itself and is
        // no view at all. A float raised over an open dropdown is the same bug
        // this function exists to fix, one layer up.
        let overrides: Vec<smithay::desktop::Window> = self
            .space
            .elements()
            .filter(|window| {
                window
                    .x11_surface()
                    .is_some_and(|x11| x11.is_override_redirect())
            })
            .cloned()
            .collect();
        for window in overrides {
            self.space.raise_element(&window, false);
        }
    }

    pub fn notify_focus(&mut self, id: u32) {
        let previous = self.focused;
        self.focused = id;

        // Every path that changes focus comes through here, including the ones
        // that only ever meant to update a list — a window closing and taking
        // focus with it, a workspace switch. Activating from here rather than
        // from each of them is what keeps the clients' idea of focus and the
        // compositor's the same.
        self.activate_view(id);

        // Outside the compositor too: a taskbar draws the focused window
        // differently, and one that is never told keeps highlighting the
        // window that had focus when it started.
        if previous != id {
            let fullscreen = self.view_is_fullscreen(previous);
            self.foreign_management_state
                .set_state(previous, false, fullscreen);
        }
        let fullscreen = self.view_is_fullscreen(id);
        self.foreign_management_state
            .set_state(id, true, fullscreen);

        let event = Event::ViewFocused { id };
        self.notify(&event);
    }

    /// Whether a window is fullscreen, as the state it was configured with
    /// says — the shell decides it, and this is where it landed.
    fn view_is_fullscreen(&self, id: u32) -> bool {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

        self.views
            .get(id)
            .and_then(|view| view.window.toplevel())
            .map(|toplevel| {
                toplevel.with_pending_state(|pending| {
                    pending.states.contains(xdg_toplevel::State::Fullscreen)
                })
            })
            .unwrap_or(false)
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    /// Set when the client connected through a socket a sandbox asked for, and
    /// carries what the sandbox said about itself. Nothing is refused on the
    /// strength of it yet — the point of the protocol is that a compositor
    /// *can* tell, and a compositor that cannot tell has no way to start.
    pub security_context: Option<smithay::wayland::security_context::SecurityContext>,
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
        // One loop to stop now. This used to have to stop GLib as well, which
        // owned the outer loop and carried on happily when only calloop was
        // told — the bug behind `--exit-after` reporting its deadline and then
        // running for ever.
        self.loop_signal.stop();
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
        // Best-effort, because a machine without a usable Vulkan device still
        // has a desktop to show. Without it the shell's frame is not copied
        // into an image of the compositor's own, so WebKit's next paint lands
        // in the buffer being sampled — the shell can flicker. That is worse
        // than the copy and better than no session, and it is said out loud
        // once rather than guessed at.
        if self.shell_renderer.is_none() && !self.shell_copy_refused {
            let make = || -> anyhow::Result<viewport_vulkan::VulkanRenderer> {
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
                    .map_err(|e| {
                        anyhow::anyhow!("opening {} for the shell: {e}", path.display())
                    })?;
                let gbm = smithay::backend::allocator::gbm::GbmDevice::new(file)
                    .map_err(|e| anyhow::anyhow!("creating a gbm device for the shell: {e}"))?;
                let allocator = smithay::backend::allocator::gbm::GbmAllocator::new(
                    gbm,
                    smithay::backend::allocator::gbm::GbmBufferFlags::RENDERING,
                );
                let renderer = viewport_vulkan::VulkanRenderer::with_allocator(&device, allocator)
                    .map_err(|e| {
                        anyhow::anyhow!("creating a vulkan renderer for the shell: {e}")
                    })?;
                Ok(renderer)
            };
            match make() {
                Ok(renderer) => self.shell_renderer = Some(renderer),
                Err(e) => {
                    tracing::warn!(
                        "no Vulkan renderer to copy the shell's frame with ({e:#}); \
                         the shell may flicker"
                    );
                    self.shell_copy_refused = true;
                }
            }
        }

        let formats: Vec<(u32, u64)> = self
            .shell_renderer
            .as_ref()
            .map(|renderer| renderer.dmabuf_formats())
            .or_else(|| {
                self.udev
                    .as_ref()
                    .map(|udev| udev.primary().renderer.dmabuf_formats())
            })
            .unwrap_or_default()
            .iter()
            // Colour only. The importable set now includes the YUV formats a
            // video decoder produces, and WebKit picks whatever it is offered:
            // a shell allocated as NV12 would be a desktop painted into a luma
            // plane, which imports without complaint and looks like a
            // greyscale smear.
            .filter(|format| !viewport_vulkan::format::is_yuv(format.code))
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
            .unwrap_or_else(|| shipped_asset("shell/index.html"));
        let console = std::env::var("VIEWPORT_LOG")
            .map(|level| level.contains("debug") || level.contains("trace"))
            .unwrap_or(false);

        let size = self.layout_size();
        anyhow::ensure!(
            size.0 > 0 && size.1 > 0,
            "the shell needs an output to size itself against"
        );

        tracing::info!("starting the shell at {url}, {}x{}", size.0, size.1);
        let shell =
            crate::shell::Shell::start(&card_path, &render_path, &formats, size, &url, console)?;
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
                    if let (Some(path), Some(udev)) = (crate::dump::target(), self.udev.as_mut()) {
                        if self.shell_owned.is_none() {
                            // The dump path is Vulkan's: it is a diagnostic
                            // for the renderer that has colour management, and
                            // teaching it a second one buys nothing.
                            if let crate::udev::Gpu::Vulkan(renderer) =
                                &mut udev.primary_mut().renderer
                            {
                                if let Err(e) = crate::dump::shell_frame(renderer, &texture, &path)
                                {
                                    tracing::error!("could not dump the shell's frame: {e:#}");
                                }
                            }
                        }
                    }
                    // The whole buffer, because WebKit's per-frame damage
                    // rectangles are not carried across the shim. Redrawing
                    // more than changed costs a composite; reporting none at
                    // all stops the output.
                    self.shell_damage.add([smithay::utils::Rectangle::from_size(
                        (
                            pending.buffer.width() as i32,
                            pending.buffer.height() as i32,
                        )
                            .into(),
                    )]);
                    // Into an image of our own, because the buffer goes back
                    // to WebKit below and WebKit will paint into it again.
                    // Sampling it after that is reading the frame the engine
                    // is drawing, which alternates with whatever it drew last
                    // — a picture that changes without the compositor asking,
                    // which is what flicker is.
                    let size: smithay::utils::Size<i32, smithay::utils::Physical> = (
                        pending.buffer.width() as i32,
                        pending.buffer.height() as i32,
                    )
                        .into();
                    // Allocated before the old one is given up, not after.
                    //
                    // The old buffer is the picture on screen. Taking it first
                    // and then failing to replace it — the layout changed and
                    // the device is out of memory, or the renderer is gone —
                    // drops the shell out of the render list entirely, which
                    // is a grey half of a desktop that comes back only if
                    // WebKit paints again. Holding a stale frame is the better
                    // failure: it is wrong by one layout, not absent.
                    let stale = match self.shell_owned.as_ref() {
                        Some((_, at)) => *at != size,
                        // First frame.
                        None => true,
                    };
                    if stale {
                        match self
                            .shell_renderer
                            .as_mut()
                            .and_then(|renderer| crate::dump::owned_image(renderer, size).ok())
                        {
                            Some(buffer) => self.shell_owned = Some((buffer, size)),
                            None => tracing::error!(
                                "could not allocate a {}x{} image for the shell's frame",
                                size.w,
                                size.h
                            ),
                        }
                    }
                    match self.shell_owned.take() {
                        // Only into a buffer the frame actually fits. The
                        // reallocation above failed if this does not match, and
                        // copying anyway would paint a new frame into part of
                        // an old one — a torn composite of two layouts, which
                        // reads as a rendering bug rather than as the
                        // allocation failure it is.
                        Some((mut buffer, at)) if at == size => {
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
                        Some(kept) => {
                            tracing::warn!(
                                "keeping the shell's last frame; this one has nowhere to go"
                            );
                            self.shell_owned = Some(kept);
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

    /// Bring the shell back after WebKit's web process died.
    ///
    /// The web process is not the compositor's, so its death is survivable —
    /// but nothing recovers on its own. WebKit leaves the view blank and stops
    /// painting, and on a desktop whose entire UI is that view the result is
    /// indistinguishable from a compositor that has hung: the last frame stays
    /// on screen forever and no click does anything.
    ///
    /// The last painted frame is deliberately left up while the reload runs.
    /// It is the compositor's own copy, not WebKit's memory, so it is safe to
    /// keep, and a transient crash then costs a second of a stale bar rather
    /// than a black screen. It is cleared only when recovery is given up on,
    /// where a frozen picture would be a lie about the state of the desktop.
    pub fn restart_shell(&mut self, reason: viewport_web::webkit::Termination) {
        use crate::shell::Recovery;

        if !reason.is_recoverable() {
            tracing::warn!("not restarting the shell: {reason}");
            return;
        }

        let attempt = crate::shell::budget(
            &mut self.shell_restarts,
            &mut self.shell_restart_window,
            std::time::Instant::now(),
        );

        let attempt = match attempt {
            Recovery::Restart(attempt) => attempt,
            Recovery::GiveUp(count) => {
                // The desktop is gone either way; what stopping preserves is a
                // machine that can still be logged into and read the log.
                tracing::error!(
                    "the shell has died {count} times in {:?}; giving up",
                    crate::shell::RESTART_WINDOW
                );
                // Dropping the copy takes the shell out of the element list,
                // and the damage tracker repaints what it covered because the
                // element it knew is gone. Nothing has to be added to
                // `shell_damage`: that bag is only read while there is a
                // buffer to describe.
                self.shell_owned = None;
                self.needs_render = true;
                return;
            }
        };

        tracing::warn!("restarting the shell after {reason} (attempt {attempt})");

        // The new process is a fresh page: it has painted nothing, said
        // nothing, and knows nothing about the layout. Everything derived from
        // the old one has to go with it, or the log claims a shell that is
        // talking and painting while the screen shows neither.
        self.shell_frames = 0;
        self.shell_announced = false;
        self.shell_size = None;

        let restarted = self.shell.as_ref().map(|shell| shell.restart());
        match restarted {
            Some(Ok(())) => {
                // Unconditionally, because `shell_size` was just cleared:
                // WebKit paints nothing into a view of no size, and a restarted
                // process that is never told its size loads the page and then
                // sits there.
                self.resize_shell();
            }
            Some(Err(e)) => tracing::error!("could not restart the shell: {e:#}"),
            None => {}
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
        tracing::info!(
            "the shell is {}x{} now, for {}",
            size.0,
            size.1,
            self.space
                .outputs()
                .map(|output| {
                    let geometry = self.space.output_geometry(output).unwrap_or_default();
                    format!(
                        "{} {}x{}{:+}{:+} {:?}",
                        output.name(),
                        geometry.size.w,
                        geometry.size.h,
                        geometry.loc.x,
                        geometry.loc.y,
                        output.current_transform()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
        if let Some(shell) = self.shell.as_ref() {
            tracing::info!("shell size {}x{}", size.0, size.1);
            shell.resize(size.0, size.1);
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

/// A window being dragged with Mod4 and a button held.
///
/// The compositor follows the pointer and the shell does the arithmetic: where
/// a window may go, and how big it may be, are questions about the layout —
/// and the layout is the shell's.
pub struct PointerDrag {
    pub id: u32,
    /// The right button rather than the left: resizing rather than moving.
    pub resize: bool,
    /// Where the pointer was when the last delta was worked out.
    pub last: smithay::utils::Point<f64, smithay::utils::Logical>,
    /// Motion too small to be worth a whole pixel yet.
    ///
    /// Kept rather than rounded away: a slow drag is a stream of fractional
    /// deltas, and rounding each one to zero is a window that does not move at
    /// all until the pointer is thrown across the desk.
    pub pending: (f64, f64),
    /// When the shell was last told, so a mouse reporting a thousand times a
    /// second does not ask for a thousand relayouts.
    pub sent: Option<std::time::Instant>,
}

/// Record one turn of client-request dispatch. Split out so the call site
/// above stays a single expression.
#[allow(dead_code)]
fn state_dispatches(log: &mut crate::udev::FrameLog, nanos: u64, messages: u64) {
    log.protocol_dispatches += 1;
    log.protocol_nanos += nanos;
    log.protocol_messages += messages;
}
