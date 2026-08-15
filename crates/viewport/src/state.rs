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
use smithay::utils::{Logical, Physical, Point, Rectangle};
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

/// Where a monitor was and how it was turned, so that unplugging it is not the
/// same as never having configured it.
///
/// The mode is here too: a connector coming back is re-scanned from scratch and
/// takes the config file's mode or the panel's preferred one, so a rate chosen
/// through `output.configure` was lost with everything else.
#[derive(Debug, Clone, PartialEq)]
pub struct RememberedOutput {
    pub x: i32,
    pub y: i32,
    pub transform: smithay::utils::Transform,
    pub scale: f64,
    pub mode: Option<smithay::output::Mode>,
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
    /// How each monitor was arranged when it was last seen, by connector name.
    ///
    /// A connector that comes back is a new output as far as the backend is
    /// concerned: it is placed to the right of everything else, in whatever
    /// order the connectors are enumerated, turned the way it left the factory.
    /// Two monitors switched off overnight therefore came back swapped, and a
    /// rotated one came back landscape. This is what they go back to.
    pub output_memory: std::collections::HashMap<String, RememberedOutput>,
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

    /// The pages the in-process engine is drawing, once they have started.
    ///
    /// A list for the same reason `shell_clients` is one: `--url` on a session
    /// with more than one monitor is that page on the first screen and the
    /// shipped desktop on the rest. Empty, or one entry, everywhere else.
    #[cfg(feature = "wpe")]
    pub shells: Vec<crate::shell::Page>,

    /// Wakes the loop when the shell posts something.
    #[cfg(feature = "wpe")]
    pub shell_ping: Option<smithay::reexports::calloop::ping::Ping>,

    /// A renderer of the compositor's own, for copying WebKit's frames into
    /// buffers it owns. Independent of the backend — see `start_shell`.
    ///
    /// The same two-armed renderer the display uses, and for the same reason.
    /// This was a `VulkanRenderer` and opened whatever Vulkan device existed,
    /// which in a virtual machine is lavapipe: it owns no DRM node, imported
    /// WebKit's buffer without complaining, copied nothing anybody could see,
    /// and left a grey desktop with frames arriving and no error anywhere. A
    /// copy is only meaningful on the device that owns the buffers.
    #[cfg(feature = "wpe")]
    pub shell_renderer: Option<crate::udev::Gpu>,
    /// What the copy's target buffer is allocated from. GLES cannot allocate a
    /// DMA-BUF itself, and asking the renderer for one is what tied this to
    /// Vulkan in the first place.
    #[cfg(feature = "wpe")]
    pub shell_allocator:
        Option<smithay::backend::allocator::gbm::GbmAllocator<smithay::backend::drm::DrmDeviceFd>>,
    /// Whether opening a renderer for the shell's copy has already failed, so
    /// it is not attempted once per frame for the rest of the session.
    #[cfg(feature = "wpe")]
    pub shell_copy_refused: bool,
    /// The host's shortcuts, held back while this nested window has the
    /// keyboard. `None` when not nested, or when the host does not offer the
    /// protocol. See `crate::capture`.
    pub capture: Option<crate::capture::Capture>,
    /// Which engine draws the desktop.
    pub shell_backend: crate::shell_backend::ShellBackend,
    /// Whether that came from the command line, which the config file must not
    /// override.
    pub shell_backend_from_flag: bool,
    /// The shell as a client of this compositor, for the backend that runs it
    /// out of process. Never set at the same time as `shell`: one desktop, one
    /// engine drawing it.
    ///
    /// A list rather than one, because `--url` on a session with more than one
    /// monitor runs the page asked for on the first screen and the shipped
    /// desktop on the rest — two processes, two pages, two rectangles. See
    /// `shell_client::plan_shells`. Everything else is one entry long.
    pub shell_clients: Vec<crate::shell_client::ClientShell>,
    /// The id the next shell process is given, so a restarted one is not
    /// mistaken for the process it replaced.
    pub next_shell_id: u32,
    /// Where the page whose message is being dispatched begins, in the
    /// layout's coordinates.
    ///
    /// A page lays out in its own document, which starts at (0, 0) wherever the
    /// page itself is; the compositor places windows in the layout. See
    /// `ipc_dispatch`, which sets this, and `apply::view_layout`, which is the
    /// reason it exists. Zero for anything that is not a shell.
    pub dispatch_origin: smithay::utils::Point<i32, Logical>,
    /// Whether a page named by `--url` spans every monitor rather than taking
    /// the first one and leaving the rest to the desktop.
    ///
    /// Off, because "that site on my monitor" is what asking for a page
    /// usually means. On for a shell being developed, which is one page across
    /// the whole desk by definition — `--url-span`.
    pub shell_url_spans: bool,
    /// The terminal drawn as the wallpaper, when one was asked for.
    ///
    /// A client like any other, drawn under the shell and given no input at
    /// all — `crate::background` says why.
    pub background_terminals: Vec<crate::background::BackgroundTerminal>,
    /// The command line they are started with, and the switch that turns the
    /// whole thing on: `None` is a desktop with an ordinary wallpaper.
    pub background_command: Option<String>,
    /// Whether the "this shell paints an opaque background" refusal has been
    /// said. Once per session, not once per monitor per layout change.
    pub background_backend_warned: bool,
    /// The monitor whose wallpaper terminal currently has the keyboard, when
    /// one does.
    ///
    /// Only ever set by `toggle_background_focus`, which is only ever reached
    /// from a keybinding or the equivalent request. Nothing else in the
    /// compositor can put focus here — see `crate::background`.
    pub background_focused: Option<String>,
    /// The view that had the keyboard before, so the same chord gives it back.
    pub focus_before_background: Option<u32>,
    /// The terminal Mod4+Return opens, resolved from the config file and the
    /// environment. Kept because `--background-terminal` with no command means
    /// "that one", and the keymap is built from it and then thrown away.
    pub terminal: String,
    /// Whether the out-of-process shell has already been told it is painting
    /// into shared memory. Once per shell, not once per frame.
    pub shell_shm_warned: bool,
    /// How many frames the shell has painted. Both backends count into this —
    /// WebKit handing over a buffer and a client committing one are the same
    /// event from here — which is what makes a rate out of it comparable
    /// across engines.
    pub shell_frames: u64,
    /// The last count and when it was taken, for turning the total into a
    /// rate. `None` until the first tick, which is the sample that has nothing
    /// to compare against.
    pub shell_rate_mark: Option<(u64, std::time::Instant)>,
    /// Whether that rate is worth a line a second in the log.
    ///
    /// Off by default: a desktop that is painting is *supposed* to paint, and
    /// a line a second saying so is noise in every log anyone reads for
    /// another reason. `VIEWPORT_SHELL_RATE=1` turns it on, which is what
    /// scripts/bench-shell.py sets.
    pub shell_rate_verbose: bool,
    /// The ids for the copies of the shell drawn *over* the windows.
    ///
    /// Their own, because the damage tracker keys on the id and one element
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
    /// The ones of those that also take the pointer, which is all of them
    /// except the bar under `auto`. See `OverlayRect::passthrough`: that bar is
    /// revealed by the same modifier every window gesture is on, so a strip of
    /// screen it floats over would otherwise stop answering clicks.
    pub shell_overlay_hits: Vec<smithay::utils::Rectangle<i32, Logical>>,

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
    /// Taking the pointer image away once it has been still for long enough.
    /// Off unless `cursor.hide_after_ms` asks for it.
    pub cursor_hide: crate::cursor::Hide,
    /// Whether that deadline is already armed, so a thousand motion events do
    /// not arm a thousand timers.
    pub cursor_hide_armed: bool,
    /// The timer it is armed on. A timerfd for the reason `frame_timer` gives,
    /// and here the reason is the whole feature: the deadline is reached
    /// precisely when nothing is happening, so a tick that needs the loop to
    /// wake for other reasons is a tick that never comes.
    pub cursor_hide_timer: Option<std::os::fd::OwnedFd>,

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
    /// Where a locked client wants the cursor to reappear once the lock ends,
    /// surface-local, with the surface that asked.
    ///
    /// Held rather than acted on: the hint takes effect *when the lock is
    /// deactivated*, not when it arrives. XWayland re-sends it on every
    /// relative motion event while a game holds the pointer, so applying it
    /// on arrival would move the cursor — and send absolute motion to a
    /// client that asked for none — thousands of times a second.
    pub cursor_position_hint: Option<(WlSurface, Point<f64, Logical>)>,
    /// How many hints have arrived, for `VIEWPORT_POINTER_DEBUG` to report
    /// every hundredth rather than every one.
    pub cursor_position_hints: u64,
    /// How many relative motions have arrived, for the same reason.
    pub pointer_motions: u64,
    /// What the motion path last decided about capture, so a change of state
    /// can be logged and a million unchanged deltas cannot.
    pub pointer_capture: Option<String>,
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
    /// The timer the GPU watchdog is armed on. Same reasoning again, and here
    /// it matters most: this tick is the fallback for a desktop where the
    /// vblank chain and the frame clock have both stopped, which is exactly the
    /// state in which nothing else will wake the loop. See [`crate::recovery`].
    pub gpu_timer: Option<std::os::fd::OwnedFd>,
    /// Whether that watchdog is already armed, so a flip on every output does
    /// not arm a timer per output.
    pub gpu_watch: bool,
    /// The timer the shell's live reload settles on, when `--watch-shell` set
    /// one up. Same reasoning again — see [`crate::shell_watch`].
    pub shell_reload_timer: Option<std::os::fd::OwnedFd>,
    /// Whether a reload is already waiting on the fallback loop timer, so a
    /// save touching twenty files queues one rather than twenty.
    pub shell_reload_pending: bool,
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
    pub kde_decoration_state: smithay::wayland::shell::kde::decoration::KdeDecorationState,
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

/// One output's elements, held to that output's own rectangle, resized to the
/// scale the whole desk is being captured at, and moved to where that output
/// sits on it.
///
/// The rescale and the move are the pair smithay's own thumbnail helper
/// composes, in the same order: the rescale is about the element's own origin,
/// so the move has to happen after it or the offset would be scaled too.
///
/// The crop between them is what a real output gets from its framebuffer for
/// nothing. An output's element list is not bounded by that output — the shell
/// is one buffer spanning the whole layout, and every monitor's frame carries
/// the whole of it, offset so that its own part lands on screen. Drawing that
/// list into a framebuffer the size of one monitor throws the rest away; the
/// desk's framebuffer is the size of *all* of them, so nothing was thrown away
/// and the first monitor's copy of the shell covered every monitor after it.
/// What that looked like was a capture of two screens where the second showed
/// the desktop and its window frames with no windows in them: the frames are
/// the shell's, and the clients were behind a picture of the desktop.
type DeskElement<R> = smithay::backend::renderer::element::utils::RelocateRenderElement<
    smithay::backend::renderer::element::utils::CropRenderElement<
        smithay::backend::renderer::element::utils::RescaleRenderElement<
            crate::render::OutputElement<R>,
        >,
    >,
>;

/// Every monitor's elements laid out side by side, and the size they cover.
type DeskElements<R> = (
    Vec<DeskElement<R>>,
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
        let kde_decoration_state = smithay::wayland::shell::kde::decoration::KdeDecorationState::new::<Self>(
            &dh,
            smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode::Server,
        );
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
                // Filled in on the way out, from the keymap as it stands by
                // then: this struct is built before a config file has been
                // read and there is nothing yet to describe.
                binds: Vec::new(),
                bar: None,
                rules: None,
                theme: None,
                gaps: None,
                border: None,
                // No widgets: the default bar, until a config file adds some.
                bar_widgets: None,
                // Absent: the default module set until a config file overrides
                // the whole right side of the bar.
                bar_items: None,
                // Off the end of a monitor carries on to the next one, which
                // is what this has always done and what sway does.
                focus_crosses_outputs: true,
                // The tree of splits the shell has always built; a dynamic
                // mode is opt-in.
                tiling_mode: None,
                // No wallpaper but the shell's own, until a config file or a
                // flag says otherwise.
                background_terminal: false,
                // And no picture behind it either: the shell's gradient until
                // a config file, a flag or `config.wallpaper` names one.
                wallpaper: None,
                wallpaper_mode: None,
            },
            shell_url: None,
            output_config: std::collections::HashMap::new(),
            output_memory: std::collections::HashMap::new(),
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
            shell_ping: None,
            capture: None,
            shell_backend: crate::shell_backend::ShellBackend::default_for_build(),
            shell_backend_from_flag: false,
            shell_clients: Vec::new(),
            next_shell_id: 0,
            shell_url_spans: false,
            dispatch_origin: (0, 0).into(),
            background_terminals: Vec::new(),
            background_command: None,
            background_backend_warned: false,
            background_focused: None,
            focus_before_background: None,
            terminal: std::env::var("VIEWPORT_TERMINAL").unwrap_or_else(|_| "foot".to_owned()),
            shell_shm_warned: false,
            #[cfg(feature = "wpe")]
            shells: Vec::new(),
            #[cfg(feature = "wpe")]
            shell_renderer: None,
            #[cfg(feature = "wpe")]
            shell_allocator: None,
            #[cfg(feature = "wpe")]
            shell_copy_refused: false,
            shell_frames: 0,
            shell_rate_mark: None,
            shell_rate_verbose: std::env::var_os("VIEWPORT_SHELL_RATE").is_some(),
            shell_overlay_ids: Vec::new(),
            shell_overlays: Vec::new(),
            shell_overlay_hits: Vec::new(),

            cursor_status: smithay::input::pointer::CursorImageStatus::default_named(),
            tablet_cursor_status: None,
            cursor_theme: crate::cursor::Theme::new(),
            cursor_warned: false,
            cursor_hide: crate::cursor::Hide::default(),
            cursor_hide_armed: false,
            cursor_hide_timer: None,
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
            cursor_position_hint: None,
            cursor_position_hints: 0,
            pointer_motions: 0,
            pointer_capture: None,
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
            gpu_timer: None,
            gpu_watch: false,
            shell_reload_timer: None,
            shell_reload_pending: false,
            barrier_quiet: 0,
            _security_context_state: security_context_state,
            _xdg_toplevel_icon_manager: xdg_toplevel_icon_manager,
            _xwayland_keyboard_grab_state: xwayland_keyboard_grab_state,
            dmabuf_state,
            xwm: None,
            xdisplay: None,
            xwayland_shell_state,
            xdg_decoration_state,
            kde_decoration_state,
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
                    //
                    // An error is one turn of dispatch that went wrong, and
                    // the desktop around it carries on — as it does for a
                    // client that could not be accepted just above. Unwinding
                    // through the event loop here would take every other
                    // client with it.
                    let messages = match unsafe { display.get_mut().dispatch_clients(state) } {
                        Ok(messages) => messages,
                        Err(e) => {
                            tracing::error!("dispatching Wayland clients: {e}");
                            return Ok(PostAction::Continue);
                        }
                    };

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
            return self.shell_under(pos);
        }
        if crate::pointer::over_overlay(&self.shell_overlay_hits, pos) {
            // The shell drew something here in front of the windows — a
            // notification, a floating bar, the screen-share chooser. It is on
            // top, so it takes the pointer; reporting the window underneath
            // would hand the click straight through it.
            return self.shell_under(pos);
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
            // Not the part of it that is cropped away. See `clipped_out`.
            if self.clipped_out(&window, pos) {
                continue;
            }
            // Where the surface is drawn, not where the window is mapped.
            //
            // A client with client-side decorations draws its shadows outside
            // the window: xdg_surface.geometry marks the real window inside a
            // larger surface, and its origin is frequently negative. The map
            // location is the window's, so surface-local coordinates have to
            // start from the surface's — which is what `Space::element_under`
            // returns and what reading the map location instead got wrong, by
            // exactly the width of the shadow.
            // And not the part of it that is merely drawn smaller than it is.
            // The client was never resized, so the coordinates it is asked
            // about are its own; without this a click on a window at 0.5 lands
            // at twice its distance from the corner, which is a pointer that
            // works in the top-left of a window and misses by more the further
            // across it you go.
            let unscaled = self.unscaled(&window, pos);
            let render_location = location - window.geometry().loc;
            if let Some((surface, at)) =
                window.surface_under(unscaled - render_location.to_f64(), WindowSurfaceType::ALL)
            {
                // What comes back is not the surface's origin, it is whatever
                // makes the *subtraction* right.
                //
                // The pointer works the surface-local position out by taking
                // this away from the real pointer position, and the real
                // pointer position is in screen coordinates while the point
                // the client should be told about is in its own. Returning the
                // surface's actual origin mixes the two: the window is found
                // correctly and then handed a coordinate off by the whole
                // scale error, which is zero at the window's corner and grows
                // across it — a client where the top-left works and nothing
                // else quite does.
                //
                // So the local point is worked out here, in the window's own
                // coordinates where it means something, and what is returned
                // is the position that yields it. At 1.0 this is exactly
                // `at + render_location`, which is what it always was.
                let local = unscaled - render_location.to_f64() - at.to_f64();
                return Some((surface, pos - local));
            }
        }
        below.or_else(|| self.shell_under(pos))
    }

    /// The topmost window at a point, skipping the parts cropped away.
    ///
    /// `Space::element_under` answers from the mapped rectangles alone, which
    /// on a scrolled strip includes columns that are on a monitor without
    /// being drawn there — see `clipped_out`. Clicking through to whatever is
    /// really underneath is what this adds.
    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<smithay::desktop::Window> {
        use smithay::desktop::space::SpaceElement;

        // Otherwise as `Space::element_under`: the bounding box rather than the
        // window's own rectangle, so a client's shadow is still part of it, and
        // then the input region, so the parts it says are not clickable are not.
        self.space
            .elements()
            .rev()
            .find(|window| {
                if self.clipped_out(window, pos) {
                    return false;
                }
                // In the window's own coordinates, so that a window merely
                // drawn smaller is tested against the space it actually
                // covers. The bounding box is the full size the `Space` holds
                // it at; against the raw pointer position it claims the screen
                // around a thumbnail as well as the thumbnail.
                let pos = self.unscaled(window, pos);
                let Some(bbox) = self.space.element_bbox(window) else {
                    return false;
                };
                if !bbox.to_f64().contains(pos) {
                    return false;
                }
                // Where the surface is drawn, not where the window is mapped —
                // the same correction `surface_under` makes below.
                let Some(location) = self.space.element_location(window) else {
                    return false;
                };
                let render_location = location - window.geometry().loc;
                window.is_in_input_region(&(pos - render_location.to_f64()))
            })
            .cloned()
    }

    /// Whether a point falls on the part of a window that is cropped away.
    ///
    /// The shell scrolls a strip by moving its columns, not by hiding them: a
    /// column scrolled off the left of one monitor keeps a rectangle, and that
    /// rectangle lands on the monitor beside it. Nothing of it is *drawn*
    /// there — `view.layout` carries a clip and the renderer crops the surface
    /// to it — but the window is still mapped in the `Space` at its full size,
    /// so every hit test found it. With the second monitor scrolled a few
    /// columns along, clicking a window on the first monitor focused an
    /// invisible column of the second instead, and the strip scrolled back to
    /// it: the click had gone to a window that is not on that screen.
    ///
    /// So the clip bounds input as well as drawing. The two have to agree —
    /// what is not on the screen cannot be clicked — and the clip is the only
    /// thing that knows where a window really is.
    /// What the shell asked this window to be *drawn* at, 1.0 for almost
    /// everything.
    fn draw_scale(&self, window: &smithay::desktop::Window) -> f64 {
        use smithay::wayland::seat::WaylandFocus;

        window
            .wl_surface()
            .as_deref()
            .and_then(|surface| self.views.find_by_surface(surface))
            .map(|view| view.scale)
            .filter(|scale| scale.is_finite() && *scale > 0.0)
            .unwrap_or(1.0)
    }

    /// A point on the screen, in the coordinates of a window drawn smaller than
    /// it is.
    ///
    /// A shrunken window is a client that has not been resized: it is painted
    /// at its own size and the renderer scales the result about the window's
    /// top-left corner (`WindowFrame::origin`, and `RescaleRenderElement` in
    /// render.rs). The `Space` still holds it at full size, because that is
    /// what it is — so every hit test asked "which window is under the
    /// pointer" of a layout drawn at one scale and stored at another, and got
    /// an answer for a window that is not what anybody can see.
    ///
    /// Undoing the scale about the same corner is the whole correction. It is
    /// identity at 1.0, which is every window in every layout that does not
    /// shrink one, so the arithmetic is skipped rather than rounded through.
    ///
    /// The corner is `element_geometry().loc`: the window's own top-left, not
    /// the surface's — a client drawing its shadows outside its geometry starts
    /// its surface some pixels up and left of the window, and scaling about
    /// *that* is what leaves a strip of window hanging outside the box the
    /// shell drew. The renderer picks the same corner for the same reason.
    pub fn unscaled(
        &self,
        window: &smithay::desktop::Window,
        pos: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        let scale = self.draw_scale(window);
        if (scale - 1.0).abs() < f64::EPSILON {
            return pos;
        }
        let Some(origin) = self
            .space
            .element_geometry(window)
            .map(|geometry| geometry.loc)
        else {
            return pos;
        };
        unscale_about(origin.to_f64(), pos, scale)
    }

    pub fn clipped_out(&self, window: &smithay::desktop::Window, pos: Point<f64, Logical>) -> bool {
        use smithay::wayland::seat::WaylandFocus;

        let Some(clip) = window
            .wl_surface()
            .as_deref()
            .and_then(|surface| self.views.find_by_surface(surface))
            .and_then(|view| view.clip)
        else {
            // No clip means nothing was cropped: the whole window is on
            // screen, which is every window on an unscrolled workspace.
            return false;
        };
        /* The clip is in the window's own coordinates — the shell divides the
        thumbnail scale back out before sending it — so a point on a shrunken
        window has to come back the same way before it is compared. */
        let pos = self.unscaled(window, pos);
        !crate::views::clip_covers(clip, pos.x, pos.y)
    }

    /// The shell's own surface at a point, for the out-of-process backend.
    ///
    /// This is the whole of its input handling. Where the WPE backend answers
    /// `None` — "the pointer is on the shell, so tell the engine directly" —
    /// this answers with a surface, and the pointer and keyboard take it from
    /// there exactly as they would for any other client. A click on a titlebar
    /// the shell drew is a `wl_pointer.button` on the shell's surface.
    ///
    /// One buffer across the whole layout, mapped at the layout's origin, so a
    /// position in layout coordinates is already surface-local.
    fn shell_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        // Where the *surface* is, not where the pointer is: smithay subtracts
        // this from the pointer's position to get the surface-local
        // coordinate. Returning the pointer's own position made every click
        // arrive at (0, 0) — the top-left corner of the page, whatever had
        // been aimed at — which is why nothing the shell drew could be
        // clicked.
        //
        // The corner of whichever page is under the pointer, not the layout's
        // origin: a desktop on its own is mapped at the origin and the two are
        // the same, and a `--url` page on the second monitor is not.
        self.shell_at(pos)
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

    /// Every monitor at once, as one picture of the desk.
    ///
    /// Built from each output's own frame rather than from a frame of its own:
    /// what belongs on a screen — which layer surfaces, which part of the
    /// shell, where the pointer is — is decided per output and there is no
    /// second answer for the desk. So each is worked out exactly as it is
    /// displayed and then moved into place, which also means a rotated monitor
    /// arrives rotated and a scaled one arrives at the desk's scale.
    ///
    /// The pointer comes out right for free: `cursor_for` draws it only on the
    /// output it is over, so exactly one of these frames carries it.
    fn desk_elements<R>(&mut self, renderer: &mut R) -> Result<DeskElements<R>, String>
    where
        R: Renderer
            + smithay::backend::renderer::ImportAll
            + smithay::backend::renderer::ImportMem
            + smithay::backend::renderer::ImportDma,
        <R as smithay::backend::renderer::RendererSuper>::TextureId: Clone + Send + Sync + 'static,
    {
        use smithay::backend::renderer::element::utils::{
            CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
        };

        let (union, scale) = self
            .all_outputs_layout()
            .ok_or_else(|| "there are no monitors".to_owned())?;
        let size = self
            .desk_size()
            .ok_or_else(|| "there are no monitors".to_owned())?;

        // Collected first: building a frame needs the whole state, and the
        // space cannot be iterated across that.
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();

        let mut elements: Vec<DeskElement<R>> = Vec::new();
        for output in outputs {
            let Some(geometry) = self.space.output_geometry(&output) else {
                continue;
            };
            let frame = self.frame_for(&output);
            // From the frame rather than from the output, because that is the
            // scale its elements were laid out at.
            let magnify = scale / frame.scale.max(f64::MIN_POSITIVE);
            // Where this monitor's picture goes, and the rectangle it is held
            // to — its own, with the monitor at the origin, because the crop
            // happens before the move. See `render::desk_placement`.
            let (bounds, at) = crate::render::desk_placement(geometry, union, scale);
            elements.extend(
                crate::render::build(&frame, renderer)
                    .into_iter()
                    .filter_map(|element| {
                        let scaled = RescaleRenderElement::from_element(
                            element,
                            smithay::utils::Point::from((0, 0)),
                            magnify,
                        );
                        // Cropped away entirely: an element of this monitor's
                        // frame that belongs to another one, which is most of
                        // the shell every time.
                        let cropped = CropRenderElement::from_element(scaled, scale, bounds)?;
                        Some(RelocateRenderElement::from_element(
                            cropped,
                            at,
                            Relocate::Relative,
                        ))
                    }),
            );
        }

        // Front to back within an output, and in the space's order between
        // them. The order between them does not matter: two monitors do not
        // overlap, so nothing on one is ever in front of anything on another.
        Ok((elements, size))
    }

    /// Composite the whole desk straight into a consumer's buffer.
    fn render_desk_into<R>(
        &mut self,
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
        let (elements, size) = self.desk_elements(renderer)?;

        let mut framebuffer = renderer
            .bind(&mut target)
            .map_err(|e| format!("binding the client's buffer: {e}"))?;
        // Upright and unscaled: every element was already placed in the desk's
        // own physical pixels, including whatever rotation its monitor has.
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
            .map_err(|e| format!("compositing the desk into the client's buffer: {e:?}"))?;

        // Waited for, because nothing else will: the client is handed the
        // buffer the GPU is still writing into, and a consumer that reads it
        // straight away sees whatever was there before.
        result
            .sync
            .wait()
            .map_err(|e| format!("waiting for the capture to finish: {e}"))
    }

    /// Composite the whole desk and read it back, for a stream that cannot be
    /// drawn into directly.
    fn read_desk_pixels<R, B>(
        &mut self,
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
        let (elements, size) = self.desk_elements(renderer)?;

        // The format it will be read back as, because the Vulkan renderer
        // refuses to convert while copying — see `read_output_pixels`.
        let format = smithay::backend::allocator::Fourcc::Xrgb8888;
        let buffer_size: smithay::utils::Size<i32, smithay::utils::Buffer> =
            (size.w, size.h).into();
        let mut target = renderer
            .create_buffer(format, buffer_size)
            .map_err(|e| format!("allocating a desk capture target: {e}"))?;

        let mapping = {
            let mut framebuffer = renderer
                .bind(&mut target)
                .map_err(|e| format!("binding a desk capture target: {e}"))?;
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
                .map_err(|e| format!("compositing the desk: {e:?}"))?;
            renderer
                .copy_framebuffer(
                    &framebuffer,
                    smithay::utils::Rectangle::from_size(buffer_size),
                    format,
                )
                .map_err(|e| format!("reading the desk back: {e}"))?
        };
        let pixels = renderer
            .map_texture(&mapping)
            .map_err(|e| format!("mapping a desk capture: {e}"))?
            .to_vec();
        Ok((pixels, size))
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
            // `wlr-randr` and the display panel of a settings app arrange
            // monitors through here rather than through `output.configure`, and
            // an arrangement made with one of those is worth restoring too.
            self.remember_output(&output);
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
            // Back where it was, from the memory — an unmapped output has no
            // geometry of its own to read. Without an entry there it goes to
            // the right of everything, which is where a newly plugged monitor
            // goes too.
            let location = self
                .output_memory
                .get(&output.name())
                .map(|remembered| (remembered.x, remembered.y))
                .unwrap_or_else(|| {
                    let x = self
                        .space
                        .outputs()
                        .filter_map(|other| self.space.output_geometry(other))
                        .map(|geometry| geometry.loc.x + geometry.size.w)
                        .max()
                        .unwrap_or(0);
                    (x, 0)
                });
            self.map_output_at(output, location);
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
                && focus.is_surface(inhibitor.wl_surface())
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

        // Resolved once here for the name and the first size, and again on
        // every frame after: a following source names whatever is in front
        // now, and what that is will have changed by the second frame.
        let target = self
            .resolve_cast(&source)
            .ok_or_else(|| anyhow::anyhow!("there is nothing to share"))?;
        let name = self.cast_name(&source, &target);
        let size = self
            .target_size(&target)
            .ok_or_else(|| anyhow::anyhow!("{name} cannot be captured"))?;

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

        // What each share names right now. Resolved once and reused, because a
        // following source is answered from focus and the answer must not
        // change between deciding to composite and deciding who receives it —
        // that is a frame handed to the wrong stream at the wrong size.
        let targets = self.cast_targets_now();

        // Then the ones that need pixels in shared memory. One composite and
        // one readback serves every client watching this output.
        let watching_output = self.casts.iter().zip(targets.iter()).any(|(cast, target)| {
            cast.stream.wants_frame(RATE)
                && !cast.stream.uses_dmabuf()
                && matches!(target, Some(crate::screencast::Target::Output(o)) if o == output)
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
                        &targets,
                        |target| {
                            matches!(target, crate::screencast::Target::Output(o) if o == output)
                        },
                        &pixels,
                        size,
                    ),
                    Err(e) => tracing::warn!("could not read a frame for a screencast: {e}"),
                }
            }
        }

        // Then the whole desk, if anything is watching it — once per frame
        // rather than once per monitor, on whichever output does the work.
        let watching_desk = self.casts.iter().zip(targets.iter()).any(|(cast, target)| {
            cast.stream.wants_frame(RATE)
                && !cast.stream.uses_dmabuf()
                && matches!(target, Some(crate::screencast::Target::AllOutputs))
        });
        if watching_desk && self.desk_capture_output().as_ref() == Some(output) {
            match self.read_desk_pixels::<R, B>(renderer) {
                Ok((pixels, size)) => self.push_to_casts(
                    &targets,
                    |target| matches!(target, crate::screencast::Target::AllOutputs),
                    &pixels,
                    size,
                ),
                Err(e) => tracing::warn!("could not read the desk for a screencast: {e}"),
            }
        }

        // Then windows, one composite each. A window is shared as itself
        // rather than as the part of the screen it covers: whatever is on top
        // of it belongs to the desktop, not to the thing being shared.
        //
        // Each window once, however many streams are watching it: a share
        // that follows the focused window and a share of that same window by
        // name both resolve here, and compositing it twice would cost a whole
        // window per extra viewer for an identical picture.
        let mut windows: Vec<u32> = self
            .casts
            .iter()
            .zip(targets.iter())
            .filter(|(cast, _)| cast.stream.wants_frame(RATE) && !cast.stream.uses_dmabuf())
            .filter_map(|(_, target)| match target {
                Some(crate::screencast::Target::Window(id)) => Some(*id),
                _ => None,
            })
            .collect();
        windows.sort_unstable();
        windows.dedup();
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
                    &targets,
                    |target| matches!(target, crate::screencast::Target::Window(other) if *other == id),
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

    /// What a source names right now.
    ///
    /// `None` is "nothing to point at just now" rather than an error: a share
    /// following the focused window has nothing to show while focus is
    /// nowhere, and the honest thing to do is leave the last frame up until
    /// there is a window again. Tearing the share down would mean a click on
    /// the desktop ended the meeting.
    fn resolve_cast(
        &self,
        source: &crate::screencast::Source,
    ) -> Option<crate::screencast::Target> {
        use crate::screencast::{Source, Target};
        match source {
            Source::Output(output) => Some(Target::Output(output.clone())),
            Source::Window(id) => self
                .views
                .get(*id)
                .filter(|view| view.mapped)
                .map(|view| Target::Window(view.id)),
            Source::AllOutputs => self.space.outputs().next().map(|_| Target::AllOutputs),
            Source::FollowOutput => self
                .active_output
                .as_ref()
                .and_then(|name| self.output_by_name(name))
                .or_else(|| self.space.outputs().next().cloned())
                .map(Target::Output),
            Source::FollowWindow => self
                .views
                .get(self.focused)
                .filter(|view| view.mapped)
                .map(|view| Target::Window(view.id)),
        }
    }

    /// What to call a stream, which is what a consumer shows in its own list.
    ///
    /// From the source rather than from what it currently resolves to: the name
    /// is fixed at negotiation and a following share that renamed itself every
    /// time focus moved would be a recorder whose file name is whatever window
    /// was in front when it stopped.
    fn cast_name(
        &self,
        source: &crate::screencast::Source,
        target: &crate::screencast::Target,
    ) -> String {
        use crate::screencast::{Source, Target};
        match (source, target) {
            (Source::AllOutputs, _) => "all monitors".to_owned(),
            (Source::FollowOutput, _) => "the active monitor".to_owned(),
            (Source::FollowWindow, _) => "the focused window".to_owned(),
            (_, Target::Output(output)) => output.name(),
            (_, Target::Window(id)) => self
                .views
                .get(*id)
                .map(|view| view.title())
                .unwrap_or_else(|| "a window".to_owned()),
            (_, Target::AllOutputs) => "all monitors".to_owned(),
        }
    }

    /// The size a source is now, whatever it was when the share started.
    fn cast_size(
        &self,
        source: &crate::screencast::Source,
    ) -> Option<smithay::utils::Size<i32, smithay::utils::Physical>> {
        self.target_size(&self.resolve_cast(source)?)
    }

    /// How big a picture of this would be.
    fn target_size(
        &self,
        target: &crate::screencast::Target,
    ) -> Option<smithay::utils::Size<i32, smithay::utils::Physical>> {
        match target {
            crate::screencast::Target::Output(output) => output
                .current_mode()
                .map(|mode| output.current_transform().transform_size(mode.size)),
            crate::screencast::Target::Window(id) => {
                let view = self.views.get(*id)?;
                let geometry = self.space.element_geometry(&view.window)?;
                Some((geometry.size.w.max(1), geometry.size.h.max(1)).into())
            }
            crate::screencast::Target::AllOutputs => self.desk_size(),
        }
    }

    /// How big a picture of the whole desk is.
    ///
    /// At least one pixel each way: an empty layout would otherwise negotiate
    /// a zero-sized stream, which PipeWire accepts and no consumer can read.
    fn desk_size(&self) -> Option<smithay::utils::Size<i32, smithay::utils::Physical>> {
        let (union, scale) = self.all_outputs_layout()?;
        let size: smithay::utils::Size<i32, smithay::utils::Physical> =
            union.size.to_f64().to_physical(scale).to_i32_round();
        Some((size.w.max(1), size.h.max(1)).into())
    }

    /// The rectangle every monitor sits inside, and the scale to draw it at.
    ///
    /// The largest scale of any of them, not the smallest and not one: the
    /// point of sharing the whole desk is that somebody watching can read what
    /// is on it, and a two-monitor desk where one screen is HiDPI would
    /// otherwise be captured with that screen halved. Oversampling the coarser
    /// monitor costs pixels; undersampling the finer one costs the text.
    fn all_outputs_layout(&self) -> Option<(Rectangle<i32, Logical>, f64)> {
        let mut union: Option<Rectangle<i32, Logical>> = None;
        let mut scale: f64 = 1.0;
        for output in self.space.outputs() {
            let Some(geometry) = self.space.output_geometry(output) else {
                continue;
            };
            scale = scale.max(output.current_scale().fractional_scale());
            union = Some(match union {
                Some(union) => union.merge(geometry),
                None => geometry,
            });
        }
        union.map(|union| (union, scale))
    }

    /// Which output does the work for a share of the whole desk.
    ///
    /// Every output's frame calls into the capture path, and a picture of the
    /// desk is the same picture whichever of them asked for it — so it is
    /// composited on one of their frames and skipped on the rest. Without this
    /// a three-monitor desk composited the whole layout three times a frame.
    ///
    /// The first in the space's order, which is stable for as long as the set
    /// of monitors is: any one of them would do, and one that moved would mean
    /// a frame missed or drawn twice each time it changed.
    fn desk_capture_output(&self) -> Option<Output> {
        self.space.outputs().next().cloned()
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

        // What each share names right now, before the casts are taken out of
        // the state: resolving a following source needs the state, and this
        // borrows it immutably while `self.casts` is still there to line up
        // with.
        let targets = self.cast_targets_now();
        // Whether a share of the whole desk is this output's job, worked out
        // for the same reason.
        let desk_is_ours = self.desk_capture_output().as_ref() == Some(output);

        // Both taken out for the duration: compositing needs the whole state,
        // and the stream being drawn into is part of it.
        let mut casts = std::mem::take(&mut self.casts);
        let pipewire = self.pipewire.take();
        if let Some(pipewire) = pipewire.as_ref() {
            for (cast, target) in casts.iter_mut().zip(targets.iter()) {
                if !cast.stream.uses_dmabuf() || !cast.stream.wants_frame(RATE) {
                    continue;
                }
                match target {
                    Some(crate::screencast::Target::Output(shared)) if shared == output => {
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
                    Some(crate::screencast::Target::Window(id)) => {
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
                    // One monitor's frame does the desk, and the rest skip it:
                    // the picture is the same whichever of them asked.
                    Some(crate::screencast::Target::AllOutputs) if desk_is_ours => {
                        let Some(size) = self.desk_size() else {
                            continue;
                        };
                        cast.stream
                            .with_target(size, &pipewire.thread_loop, |target| {
                                self.render_desk_into(target.clone(), renderer)
                            });
                    }
                    _ => {}
                }
            }
        }
        self.pipewire = pipewire;
        self.casts = casts;
    }

    /// What every running share names right now, in `self.casts` order.
    ///
    /// Kept alongside the casts rather than inside them: a following source is
    /// answered from the compositor's state, and storing the answer would mean
    /// deciding when to refresh it. This way there is nothing to keep in step.
    fn cast_targets_now(&self) -> Vec<Option<crate::screencast::Target>> {
        self.casts
            .iter()
            .map(|cast| self.resolve_cast(&cast.source))
            .collect()
    }

    /// Hand a frame to every cast a predicate matches.
    ///
    /// Matched on what each share resolves to rather than on what it asked
    /// for, so a stream following the focused window is fed by the same
    /// composite that feeds a stream naming that window outright. `targets`
    /// runs alongside `self.casts`; a share that resolves to nothing is fed
    /// nothing.
    fn push_to_casts(
        &mut self,
        targets: &[Option<crate::screencast::Target>],
        matches: impl Fn(&crate::screencast::Target) -> bool,
        pixels: &[u8],
        size: smithay::utils::Size<i32, smithay::utils::Physical>,
    ) {
        let mut casts = std::mem::take(&mut self.casts);
        if let Some(pipewire) = self.pipewire.as_ref() {
            for (cast, target) in casts.iter_mut().zip(targets.iter()) {
                let feed = !cast.stream.uses_dmabuf() && target.as_ref().is_some_and(&matches);
                if feed {
                    cast.stream.push(pixels, size, &pipewire.thread_loop);
                }
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
    ///
    /// Nothing at all while the session is locked, which every caller composites
    /// as a black frame of the right size. A screen is blanked for a lock by
    /// `frame_for` — see `Frame::locked_blank` — and a window is not drawn
    /// through that at all: it is its own surface tree, composited here, so a
    /// share of a window went on streaming what was in it across the lock
    /// screen. A share that stops rather than freezes: the last frame before
    /// the lock is as much of the desktop as the next one would be.
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
        if self.locked {
            return Ok((Vec::new(), size));
        }

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
            Message::Start {
                types,
                restore,
                reply,
            } => self.open_screencast_picker(types, restore, reply),
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
        restore: Option<crate::screencast::Remembered>,
        reply: async_channel::Sender<Result<crate::screencast::portal::Started, String>>,
    ) {
        // One at a time. Two choosers on screen with one keyboard between them
        // is a race the user cannot see, let alone win.
        if self.picker.is_some() {
            let _ = reply.try_send(Err("something else is already being chosen".to_owned()));
            return;
        }

        // The same thing as last time, if the application asked for that and
        // the thing is still there. Before the chooser rather than as a row in
        // it: the point of a remembered share is that a recorder set up in
        // March still records the right screen in June without anybody at the
        // keyboard, and a chooser that has to be answered is exactly what the
        // application asked to avoid.
        if let Some(remembered) = restore {
            match self.restore_source(&remembered, types) {
                Some(source) => {
                    // The remembered form rather than the source: an `Output`
                    // prints its every mode and instance, and a line nobody
                    // can read in a log is a line that is not there.
                    tracing::info!("sharing {remembered:?} again, as the application asked");
                    let _ = reply.try_send(self.begin_cast(source));
                    return;
                }
                // Not a failure. The monitor is unplugged or the window is
                // closed, and the honest answer is the chooser — sharing some
                // other screen because it is the one left would hand over a
                // desk nobody agreed to.
                None => tracing::info!(
                    "the application asked to share {remembered:?} again, \
                     which is not here, so asking"
                ),
            }
        }

        let sources = self.screencast_sources(types);
        if sources.is_empty() {
            let _ = reply.try_send(Err("there is nothing to share".to_owned()));
            return;
        }

        // Nobody to draw it. A shell that is not up — a test, a crash — should
        // still be able to share a screen, so this falls back to what was on
        // screen when the user pressed share.
        //
        // Asked of the page rather than of `shell_is_up`, which answers a
        // narrower question and answered it wrong here: see `shell_can_draw`.
        if !self.shell_can_draw() {
            let source = sources.into_iter().next().expect("checked above");
            // Said out loud, because from outside it looks like the chooser
            // was skipped for no reason — which is how this went unnoticed on
            // every shipped build. A share that was never asked about is worth
            // a line in the log whatever the reason.
            tracing::info!(
                "no desktop page is drawing, so sharing {} without asking",
                source.describe()
            );
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
            keyboard.set_focus(
                self,
                Option::<crate::keyboard_focus::KeyboardFocus>::None,
                serial,
            );
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

    /// Whether there is an engine in this process to be sent input.
    ///
    /// Narrower than it sounds, and deliberately false on every shipped build:
    /// only the `wpe` backend runs the page inside the compositor, and only
    /// that one needs pointer and key events forwarded to it by hand. The
    /// out-of-process backends are Wayland clients and receive their own.
    ///
    /// Which makes this the wrong question to ask about *drawing*, and asking
    /// it was a bug — see `shell_can_draw`.
    pub fn shell_is_up(&self) -> bool {
        #[cfg(feature = "wpe")]
        {
            !self.shells.is_empty()
        }
        #[cfg(not(feature = "wpe"))]
        {
            false
        }
    }

    /// Whether there is a desktop page on screen, whichever backend draws it.
    ///
    /// The question anything the shell has to *show* must ask. `shell_is_up`
    /// is about input and is false for every backend except `wpe`, so the
    /// screen-share chooser — which asked it — never appeared on any shipped
    /// build: `packages.default` is `cef`, and every request fell through to
    /// the no-shell fallback and shared the focused window without asking.
    /// That is a screen handed over on the strength of a keystroke nobody
    /// made, which is exactly what the chooser exists to prevent.
    ///
    /// A committed buffer rather than a live process: a page that has started
    /// and not yet painted cannot show anything either, and a chooser sent to
    /// one is a share that hangs until the timeout.
    pub fn shell_can_draw(&self) -> bool {
        #[cfg(feature = "wpe")]
        if self
            .shells
            .iter()
            .any(|page| page.desktop && page.owned.is_some())
        {
            return true;
        }
        self.shell_clients
            .iter()
            .any(|page| page.desktop && page.owned.is_some())
    }

    /// Everything the application could be given a picture of.
    ///
    /// Windows before monitors, and the focused window first: what somebody
    /// means to share is usually what they were just looking at, and the list
    /// is walked from the top.
    ///
    /// The sources that name nothing in particular — the whole desk, the
    /// focused window, the active monitor — come at the end of the group they
    /// belong to rather than at the top of it. They are the more useful answer
    /// for a long meeting and the more surprising one for a quick share, and
    /// the top of the list is what Enter picks without reading.
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
            // Only when there is a window to follow. Offered on an empty
            // desktop it is a choice that shares a black rectangle until
            // somebody opens something.
            if focused.is_some() {
                sources.push(crate::screencast::Source::FollowWindow);
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
            // Both of these are the one monitor there is on a laptop, so they
            // are offered only where they differ from a row already in the
            // list. Two rows that share the same picture is a choice that is
            // not one.
            if self.space.outputs().count() > 1 {
                sources.push(crate::screencast::Source::AllOutputs);
                sources.push(crate::screencast::Source::FollowOutput);
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
                // Said in full here rather than left to the shell to name: the
                // difference between sharing a monitor and sharing whichever
                // monitor you are on is the whole of what somebody is agreeing
                // to, and it has to be readable in the row.
                crate::screencast::Source::AllOutputs => viewport_ipc::CastSource {
                    kind: "all-outputs".to_owned(),
                    label: "All monitors".to_owned(),
                    detail: format!("{} screens, side by side", self.space.outputs().count()),
                },
                crate::screencast::Source::FollowWindow => viewport_ipc::CastSource {
                    kind: "follow-window".to_owned(),
                    label: "The focused window".to_owned(),
                    detail: "follows as you switch windows".to_owned(),
                },
                crate::screencast::Source::FollowOutput => viewport_ipc::CastSource {
                    kind: "follow-output".to_owned(),
                    label: "The active monitor".to_owned(),
                    detail: "follows as you move between screens".to_owned(),
                },
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
        // Written down before the share starts, because starting it takes the
        // source, and what a window is called has to be read while there is
        // still a window to ask.
        let remembered = self.remember_cast(&source);
        self.start_cast(source)
            .map(|(node, size)| crate::screencast::portal::Started {
                node,
                width: size.w,
                height: size.h,
                source_type,
                remembered,
            })
            .map_err(|e| e.to_string())
    }

    /// Say what is being shared in terms that outlive it.
    ///
    /// A window whose id names nothing is not written down: a token that
    /// restores to an empty app id and an empty title would match the first
    /// nameless window on the desk next time, which is a share of whatever
    /// happens to be open rather than of what was agreed to.
    fn remember_cast(
        &self,
        source: &crate::screencast::Source,
    ) -> Option<crate::screencast::Remembered> {
        use crate::screencast::{Remembered, Source};
        match source {
            Source::Output(output) => Some(Remembered::Output(output.name())),
            Source::Window(id) => {
                let view = self.views.get(*id)?;
                let (app_id, title) = (view.app_id(), view.title());
                (!app_id.is_empty() || !title.is_empty())
                    .then_some(Remembered::Window { app_id, title })
            }
            Source::AllOutputs => Some(Remembered::AllOutputs),
            Source::FollowWindow => Some(Remembered::FollowWindow),
            Source::FollowOutput => Some(Remembered::FollowOutput),
        }
    }

    /// Turn what was written down back into something to share, or nothing.
    ///
    /// Checked against what the application asked for as well as against what
    /// is on the desk: a token minted when a browser wanted either kind comes
    /// back when it wants only a window, and handing it a monitor because that
    /// is what it shared last time is a screen shared by a tab that asked for
    /// a tab.
    fn restore_source(
        &self,
        remembered: &crate::screencast::Remembered,
        types: u32,
    ) -> Option<crate::screencast::Source> {
        use crate::screencast::{Remembered, Source};
        if types & remembered.kind() == 0 {
            return None;
        }

        let source = match remembered {
            Remembered::Output(name) => Source::Output(self.output_by_name(name)?),
            Remembered::Window { app_id, title } => {
                Source::Window(self.remembered_window(app_id, title)?)
            }
            Remembered::AllOutputs => Source::AllOutputs,
            Remembered::FollowWindow => Source::FollowWindow,
            Remembered::FollowOutput => Source::FollowOutput,
        };
        // And that there is something behind it now. A following source with
        // nothing to follow starts a stream that would show a black rectangle
        // until somebody opened a window, which is worse than being asked.
        self.resolve_cast(&source).map(|_| source)
    }

    /// The window a remembered share meant, if it is still open.
    ///
    /// The choosing is `screencast::matching_window`, which is where it can be
    /// tested without a compositor around it.
    fn remembered_window(&self, app_id: &str, title: &str) -> Option<u32> {
        let open: Vec<_> = self
            .views
            .iter()
            .filter(|view| view.mapped)
            .map(|view| crate::screencast::Open {
                id: view.id,
                app_id: view.app_id(),
                title: view.title(),
            })
            .collect();
        crate::screencast::matching_window(app_id, title, &open)
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
        if self.shell_frames > 0 || self.shells.is_empty() {
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
        // Every page, because the deadline is on the session painting at all:
        // if nothing has, none of them is showing anything to lose.
        for page in &self.shells {
            // Fire and forget: the load happens on the web thread, so there is
            // no error to catch here. One that fails says so in the log from
            // there.
            page.engine.load(&url);
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
                    // Rounded like any other window. The rescue layout draws
                    // no frame of its own, but the corner comes from the
                    // config and a broken shell is no reason to change it.
                    square: false,
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

    /// How fast the shell is painting, as a rate rather than a total.
    ///
    /// The total says whether the shell ever painted, which is what it was
    /// added for. It does not say whether the desktop is keeping up: a shell
    /// producing four frames a second and one producing sixty look identical
    /// in a counter that only goes up, and they are not the same desktop.
    ///
    /// Counted here rather than in the render path because this is the shell's
    /// own rate — the frames it produced — and not the compositor's. A frame
    /// the shell painted that was superseded before anything drew it still
    /// cost the engine what it cost.
    pub fn report_shell_rate(&mut self) {
        let now = std::time::Instant::now();
        let Some((previous, at)) = self.shell_rate_mark else {
            // The first tick has nothing to compare against.
            self.shell_rate_mark = Some((self.shell_frames, now));
            return;
        };
        let elapsed = now.duration_since(at).as_secs_f64();
        if elapsed < 0.5 {
            return;
        }
        let painted = self.shell_frames.saturating_sub(previous);
        self.shell_rate_mark = Some((self.shell_frames, now));

        if !self.shell_rate_verbose {
            // Still worth a line for anyone already reading at debug, and
            // still not worth one for a desktop nobody is touching.
            if painted > 0 {
                tracing::debug!("shell: {:.1} frames/s", painted as f64 / elapsed);
            }
            return;
        }
        // Zero included when this is on: "the shell painted nothing for four
        // seconds" is the interesting half of a paint rate, and a series with
        // the gaps left out cannot show it.
        tracing::info!(
            "shell: {:.1} frames/s ({painted} in {elapsed:.2}s)",
            painted as f64 / elapsed
        );
    }

    /// Work out what is under the pointer again, without it having moved.
    ///
    /// A pointer's focus is decided when it moves. Anything else that changes
    /// what is under it — the shell drawing a notification over a window, a
    /// window opening beneath it, the overview coming up — leaves that focus
    /// describing a desktop that no longer exists, and the next click goes
    /// wherever the last motion said.
    ///
    /// Sent as a motion to the position it is already at, which is how a
    /// Wayland compositor says "the same pointer, somewhere else in the
    /// stack": clients receive leave and enter as they should, and one that
    /// tracks the pointer sees no jump because there is none.
    pub fn refresh_pointer_focus(&mut self) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let location = pointer.current_location();
        let under = self.surface_under(location);
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let time = self.start_time.elapsed().as_millis() as u32;
        pointer.motion(
            self,
            under,
            &smithay::input::pointer::MotionEvent {
                location,
                serial,
                time,
            },
        );
        pointer.frame(self);
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
            mounts: sample
                .mounts
                .into_iter()
                .map(|m| viewport_ipc::event::MountUsage {
                    path: m.path,
                    free: m.free,
                    total: m.total,
                })
                .collect(),
            // -1 rather than absent, matching cpu/memory: the widget tests for
            // it the same way.
            volume: sample.volume.unwrap_or(-1.0),
            muted: sample.muted.unwrap_or(false),
            // -1 rather than absent, matching volume: the mic widget tests
            // for it the same way.
            mic_volume: sample.mic_volume.unwrap_or(-1.0),
            mic_muted: sample.mic_muted.unwrap_or(false),
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

    /// The pointer was used, so the hide deadline starts again.
    ///
    /// Called for pointer, tablet and touch input and for nothing else.
    /// Typing deliberately does not count: someone writing with the mouse
    /// parked over the text is exactly who asked for this, and waking the
    /// cursor on every keystroke would leave it up the whole time.
    pub fn cursor_activity(&mut self) {
        if !self.cursor_hide.wanted() {
            return;
        }
        if self.cursor_hide.activity() {
            // It was off the screen. Nothing else would draw it: this
            // compositor draws on damage, and there is none.
            self.needs_render = true;
        }
        self.arm_cursor_hide();
    }

    /// Start the countdown, if it is not already running.
    ///
    /// Armed once and re-armed by its own expiry rather than on every motion
    /// event — see [`crate::cursor::Tick::Wait`].
    pub fn arm_cursor_hide(&mut self) {
        if self.cursor_hide_armed {
            return;
        }
        let Some(after) = self.cursor_hide.after() else {
            return;
        };
        self.arm_cursor_hide_in(after);
    }

    /// Arm it for a particular stretch: the whole deadline, or what a tick
    /// that came around early found was left of it.
    fn arm_cursor_hide_in(&mut self, after: std::time::Duration) {
        if self.cursor_hide_timer.is_none() {
            self.cursor_hide_timer = self.create_tick("cursor hide", Self::cursor_hide_tick);
        }
        if Self::arm_tick("cursor hide", self.cursor_hide_timer.as_ref(), after) {
            self.cursor_hide_armed = true;
            return;
        }

        // No timerfd. calloop's own timer still works whenever calloop is the
        // one waiting, which is every backend except the web engine's.
        self.cursor_hide_armed = true;
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(after);
        if let Err(e) = self.loop_handle.insert_source(timer, move |_, _, state| {
            state.cursor_hide_tick();
            smithay::reexports::calloop::timer::TimeoutAction::Drop
        }) {
            tracing::warn!("cursor hide: {e}");
            self.cursor_hide_armed = false;
        }
    }

    /// One turn of the hide deadline: take the pointer away, or wait out what
    /// is left of it.
    fn cursor_hide_tick(&mut self) {
        self.cursor_hide_armed = false;
        let elapsed = self.cursor_hide.idle_for();
        match self.cursor_hide.tick(elapsed) {
            crate::cursor::Tick::Hide => {
                tracing::debug!("the pointer has been still for {elapsed:?}; hiding it");
                // The image is gone as far as `cursor_for` is concerned, but
                // what is on the screen is the last frame, which has it. One
                // more frame is what actually removes it.
                self.needs_render = true;
            }
            crate::cursor::Tick::Wait(left) => {
                self.arm_cursor_hide_in(left);
            }
            crate::cursor::Tick::Nothing => {}
        }
    }

    /// Write down where an output is and how it is turned.
    ///
    /// Every deliberate arrangement goes through here: the config file, the
    /// shell's `output.configure`, wlr-output-management, and the first sight
    /// of a connector. What is not written down is the placement the backend
    /// invents while bringing a monitor up, which is the thing being corrected.
    pub fn remember_output(&mut self, output: &Output) {
        let Some(geometry) = self.space.output_geometry(output) else {
            // Off, or not mapped yet. Its old entry is the one worth keeping:
            // that is where it goes when it comes back.
            return;
        };
        self.output_memory.insert(
            output.name(),
            RememberedOutput {
                x: geometry.loc.x,
                y: geometry.loc.y,
                transform: output.current_transform(),
                scale: output.current_scale().fractional_scale(),
                mode: output.current_mode(),
            },
        );
    }

    /// Put the monitors back the way they were, and note down the ones being
    /// seen for the first time.
    ///
    /// A connector that comes back is a brand new output to the backend: it is
    /// mapped to the right of everything else in connector-enumeration order,
    /// with a default transform and a freshly picked mode. Nothing before this
    /// undid that, so monitors left unplugged for a while came back in the
    /// wrong order and the wrong orientation, and stayed that way until the
    /// session was restarted.
    ///
    /// Through the same path `output.configure` takes, so a restored rotation
    /// resizes the layer map and reaches the shell exactly as a fresh one does.
    /// Run before [`Self::apply_output_config`], so a file that names a
    /// position still has the last word.
    pub fn restore_output_layout(&mut self) {
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        for output in outputs {
            let name = output.name();
            let Some(want) = self.output_memory.get(&name).cloned() else {
                // Never seen before. Where it landed is what it goes back to.
                self.remember_output(&output);
                continue;
            };

            let geometry = self.space.output_geometry(&output).unwrap_or_default();
            let current = RememberedOutput {
                x: geometry.loc.x,
                y: geometry.loc.y,
                transform: output.current_transform(),
                scale: output.current_scale().fractional_scale(),
                mode: output.current_mode(),
            };
            if current == want {
                // Already right, which is every output that did not move. A
                // modeset for one of those is a black screen for no reason.
                continue;
            }

            tracing::info!("{name}: back to {},{} {:?}", want.x, want.y, want.transform);
            // Only a mode this display actually advertises. The memory is kept
            // by connector name, which is the only identity a connector has
            // here, so what comes back on a port is not necessarily the panel
            // that left it — and asking a different monitor for the old one's
            // modeline is a custom mode it may well refuse.
            let mode = want.mode.filter(|mode| {
                output.modes().into_iter().any(|candidate| {
                    candidate.size == mode.size && candidate.refresh == mode.refresh
                })
            });
            let request = viewport_ipc::request::OutputConfigure {
                name: name.clone(),
                enabled: None,
                mode: mode.map(|mode| viewport_ipc::request::ModeRequest {
                    width: mode.size.w,
                    height: mode.size.h,
                    refresh: mode.refresh,
                }),
                scale: Some(want.scale),
                transform: Some(crate::apply::from_smithay_transform(want.transform)),
                adaptive_sync: None,
                x: Some(want.x),
                y: Some(want.y),
            };
            crate::apply::apply(self, viewport_ipc::Request::OutputConfigure(request));
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
        use smithay::wayland::seat::WaylandFocus as _;
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
                    .wl_surface()
                    .as_deref()
                    .and_then(|surface| self.views.find_by_surface(surface));
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
                // Which output the shell drew this window on, kept before the
                // placement shadows it: the frame below is only drawn on that
                // one. See `render::frame_on_output`.
                let drawn_on_this_output = crate::render::frame_on_output(clip, output_geometry);
                let (location, clip) =
                    crate::render::window_placement(window, layout, output_geometry, clip, scale);

                // The shell's border for this window, where it has said one
                // has to be drawn above whatever is underneath — as four
                // sides around the hole rather than one rectangle over it.
                let overlay: Vec<_> = view
                    .filter(|_| drawn_on_this_output)
                    .and_then(|view| view.frame.map(|frame| (frame, view.box_)))
                    .map(|(frame, hole)| {
                        crate::render::border_sides(frame, hole)
                            .into_iter()
                            .zip(overlay_ids.iter().cloned())
                            .filter_map(|(side, id)| {
                                // Held to this output: see `overlay_side` for
                                // what happens to a border that is not.
                                let local = crate::render::overlay_side(side, output_geometry)?;
                                Some((id, local.to_f64().to_physical(scale).to_i32_round()))
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

                // The corner the shell drew, in physical pixels on this
                // output. Some windows have none: a fullscreen one, where the
                // stylesheet takes the border and the radius off and a rounded
                // video would be four notches of wallpaper in the corners of
                // the monitor, and a window smart radius has squared for the
                // same reason.
                let border = self.config.border.as_ref();
                // Square when the shell says so — smart radius, which is the
                // shell's call because only it knows the window is alone on
                // its workspace — or when the window is fullscreen, which the
                // stylesheet also draws without a border or a corner.
                let square = view.is_some_and(|view| view.square || view.wants_fullscreen());
                let radius = if square {
                    0
                } else {
                    border
                        .and_then(|border| border.radius)
                        .unwrap_or(crate::config::DEFAULT_BORDER_RADIUS)
                };
                // How much tighter the client's own corner is than the frame
                // around it. Configured, because the frame's thickness is.
                let width = border
                    .and_then(|border| border.width)
                    .unwrap_or(crate::config::DEFAULT_BORDER_WIDTH);
                let physical = |logical: i32| (f64::from(logical) * scale).round() as i32;
                // The box the shell drew, before any thumbnail scale: the
                // element rounding it is wrapped by the one that shrinks it,
                // so a corner is described once at full size.
                let box_ = smithay::utils::Rectangle::<i32, Physical>::new(
                    origin,
                    layout.size.to_f64().to_physical(scale).to_i32_round(),
                );
                let rounded = (radius > width).then(|| (box_, physical(radius - width)));
                // The outside of the same corner, for the border sides drawn
                // above the windows underneath a floating one.
                let overlay_rounded = (radius > 0 && !overlay.is_empty())
                    .then(|| {
                        view.and_then(|view| view.frame).map(|frame| {
                            let frame = smithay::utils::Rectangle::<i32, Logical>::new(
                                (
                                    frame.x - output_geometry.loc.x,
                                    frame.y - output_geometry.loc.y,
                                )
                                    .into(),
                                (frame.width, frame.height).into(),
                            );
                            (
                                frame.to_f64().to_physical(scale).to_i32_round(),
                                physical(radius),
                            )
                        })
                    })
                    .flatten();

                Some(crate::render::WindowFrame {
                    window: window.clone(),
                    location,
                    origin,
                    clip,
                    rounded,
                    overlay_rounded,
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

        // Whichever backend painted it: a DMA-BUF, a rectangle in the layout
        // and a damage bag, which is all the element below needs. It does not
        // care whether WebKit handed the buffer over through an engine call or
        // a client attached it to a surface.
        //
        // Every page is placed by its own rectangle rather than at the layout's
        // origin. A desktop on its own spans the layout and the two are the
        // same; a `--url` page given one screen is not.
        let placed = |buffer: &smithay::backend::allocator::dmabuf::Dmabuf,
                      region: smithay::utils::Rectangle<i32, Logical>,
                      damage: smithay::backend::renderer::utils::DamageSnapshot<
            i32,
            smithay::utils::Buffer,
        >,
                      id: smithay::backend::renderer::element::Id| {
            crate::render::Shell {
                buffer: buffer.clone(),
                location: (
                    (region.loc.x - output_geometry.loc.x) as f64 * scale,
                    (region.loc.y - output_geometry.loc.y) as f64 * scale,
                )
                    .into(),
                damage,
                id,
            }
        };

        // Both backends' pages, in one list: only one of them is ever running.
        let mut drawn: Vec<(bool, crate::render::Shell)> = Vec::new();
        #[cfg(feature = "wpe")]
        for page in &self.shells {
            if let Some((buffer, _)) = page.owned.as_ref() {
                drawn.push((
                    page.desktop,
                    placed(
                        buffer,
                        page.region,
                        page.damage.snapshot(),
                        page.element_id.clone(),
                    ),
                ));
            }
        }
        for page in &self.shell_clients {
            if let Some((buffer, _)) = page.owned.as_ref() {
                drawn.push((
                    page.desktop,
                    placed(
                        buffer,
                        page.region,
                        page.damage.snapshot(),
                        page.element_id.clone(),
                    ),
                ));
            }
        }
        // The desktop page is the one the overlays are cropped out of, so it is
        // the one that goes in `shell`; the rest are drawn beside it, under
        // everything, at their own corners.
        let mut shell = None;
        let mut pages = Vec::new();
        for (desktop, element) in drawn {
            if desktop && shell.is_none() {
                shell = Some(element);
            } else {
                pages.push(element);
            }
        }

        // This monitor's wallpaper terminal, at this monitor's own origin.
        //
        // Not placed like the shell, which is one buffer across the layout and
        // is offset by where the output starts in it. There is a terminal per
        // screen, each configured to that screen's size, so each one begins at
        // the corner of the screen it belongs to.
        let background = self
            .background_surface_for(output)
            .cloned()
            .map(|surface| (surface, Point::from((0, 0))));

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
            pages,
            background,
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
        // Still for long enough that the deadline took it away. Before the
        // client's own image is looked at, because the setting is about the
        // pointer being on the screen at all — a text field's I-beam parked
        // over a film is the same thing it is there to remove.
        if self.cursor_hide.hidden() {
            return crate::render::Cursor::Hidden;
        }
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
                // Whose dialog this is, resolved against the same list being
                // walked — a reloading shell rebuilds its layout from these
                // and needs the parent links as much as a live one does.
                Event::ViewAdded(v.added(output, true, self.views.parent_id_of(v)))
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
        // The keymap as it actually stands, rather than as anything is
        // assumed to be. A few chords exist only in one layout and a config
        // file may add or shadow any of them, so this is the one place that
        // knows — and the shell showing a list of its own would be describing
        // a keyboard nobody has.
        let mut config = self.config.clone();
        config.binds = self
            .bindings
            .iter()
            .map(|binding| viewport_ipc::event::Bind {
                chord: binding.chord(),
                action: binding.action_text(),
                mode: binding.mode.clone(),
            })
            .collect();
        let event = Event::Config(config);
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
            const LAYOUTS: [&str; 5] = ["tiling", "scrolling", "solar", "matrix", "canvas"];
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
            const MODES: [&str; 5] = ["manual", "master-stack", "spiral", "bsp", "grid"];
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
        // The wallpaper, resolved here and not in the shell: the page is handed
        // a URL it can put straight in a `url()`, and a path that is not there
        // is said out loud at load rather than becoming a background-image that
        // quietly fails to fetch inside a web view nobody can open a console
        // on.
        //
        // A bad path is a warning and not a refusal to start. Every other key
        // in this file is a preference and this one is decoration; a session
        // that will not come up because a picture was moved is worse than a
        // session that comes up with the gradient it always had.
        if let Some(wallpaper) = file.wallpaper.as_deref() {
            // The empty string is how a file takes one away again, rather than
            // a null that a reload could not tell from an absent key.
            if wallpaper.trim().is_empty() {
                self.config.wallpaper = None;
            } else {
                match crate::config::wallpaper_value(wallpaper, "wallpaper") {
                    Ok(url) => self.config.wallpaper = Some(url),
                    Err(e) => tracing::warn!("{e}; keeping the current wallpaper"),
                }
            }
        }
        if let Some(mode) = file.wallpaper_mode.as_deref() {
            match crate::config::parse_wallpaper_mode(mode) {
                Ok(mode) => self.config.wallpaper_mode = Some(mode),
                Err(e) => tracing::warn!("{e}"),
            }
        }
        if file.gaps != crate::config::GapsConfig::default() {
            // Only fields the file actually names are forwarded; an absent one
            // leaves the shell's own default. A gap of zero is a deliberate
            // request (no spacing at all), so values are forwarded as-is
            // rather than skipped for being small.
            self.config.gaps = Some(viewport_ipc::event::Gaps {
                inner: file.gaps.inner,
                outer: file.gaps.outer,
                smart: file.gaps.smart,
            });
        }
        if file.border != crate::config::BorderConfig::default() {
            self.config.border = Some(viewport_ipc::event::Border {
                radius: file.border.radius,
                width: file.border.width,
                smart: file.border.smart,
            });
        }
        // The bar. Two ways to ask for it: `bar_widgets` adds widgets to the
        // default module set; `bar_items` overrides the entire right side of
        // the bar with an explicit, ordered list of modules and widgets. When
        // `bar_items` is present (even empty) it wins outright — the shell
        // draws exactly what it lists and nothing else.
        //
        // The status sampler is told the same what-to-read as the shell lists:
        // which mounts to stat and whether to ask wpctl for the sink, so a bar
        // that draws neither spawns nothing.
        let bar_widgets: Vec<viewport_ipc::event::BarWidget> = file
            .bar_widgets
            .iter()
            .map(|w| match w {
                crate::config::BarWidgetConfig::Disk { path } => {
                    viewport_ipc::event::BarWidget::Disk { path: path.clone() }
                }
                crate::config::BarWidgetConfig::Weather { location } => {
                    viewport_ipc::event::BarWidget::Weather {
                        location: location.clone(),
                    }
                }
                crate::config::BarWidgetConfig::Volume => viewport_ipc::event::BarWidget::Volume,
                crate::config::BarWidgetConfig::Mic => viewport_ipc::event::BarWidget::Mic,
            })
            .collect();

        // The bar_items list, mapped to the IPC form. Bare strings are
        // modules; objects are widgets.
        let bar_items = file.bar_items.as_ref().map(|items| {
            items
                .iter()
                .map(|item| match item {
                    crate::config::BarItemConfig::Module(name) => {
                        viewport_ipc::event::BarItem::Module(name.clone())
                    }
                    crate::config::BarItemConfig::Widget(w) => {
                        viewport_ipc::event::BarItem::Widget(match w {
                            crate::config::BarWidgetConfig::Disk { path } => {
                                viewport_ipc::event::BarWidget::Disk { path: path.clone() }
                            }
                            crate::config::BarWidgetConfig::Weather { location } => {
                                viewport_ipc::event::BarWidget::Weather {
                                    location: location.clone(),
                                }
                            }
                            crate::config::BarWidgetConfig::Volume => {
                                viewport_ipc::event::BarWidget::Volume
                            }
                            crate::config::BarWidgetConfig::Mic => {
                                viewport_ipc::event::BarWidget::Mic
                            }
                        })
                    }
                })
                .collect()
        });

        // Which widgets actually get drawn: the override's own widgets when
        // present, else the bar_widgets additions. The sampler only pays for
        // what will be on screen.
        let drawn_widgets: Vec<&crate::config::BarWidgetConfig> =
            if let Some(items) = &file.bar_items {
                items
                    .iter()
                    .filter_map(|item| match item {
                        crate::config::BarItemConfig::Widget(w) => Some(w),
                        crate::config::BarItemConfig::Module(_) => None,
                    })
                    .collect()
            } else {
                file.bar_widgets.iter().collect()
            };

        self.config.bar_widgets = if file.bar_items.is_some() {
            // Superseded: the whole right side comes from bar_items, so the
            // shell is told the override and not the additions it replaces.
            None
        } else if bar_widgets.is_empty() {
            None
        } else {
            Some(bar_widgets)
        };
        self.config.bar_items = bar_items;
        self.status.configure(
            drawn_widgets
                .iter()
                .filter_map(|w| match w {
                    crate::config::BarWidgetConfig::Disk { path } => {
                        Some(path.clone().unwrap_or_else(|| "/".to_owned()))
                    }
                    _ => None,
                })
                .collect(),
            drawn_widgets
                .iter()
                .any(|w| matches!(w, crate::config::BarWidgetConfig::Volume)),
            drawn_widgets
                .iter()
                .any(|w| matches!(w, crate::config::BarWidgetConfig::Mic)),
        );
        if let Some(url) = file.url {
            self.shell_url = Some(url);
        }
        if let Some(span) = file.url_span {
            self.shell_url_spans = span;
        }
        // Only where the command line said nothing: a flag is a decision made
        // for this run, and a config file that could override it would make
        // `--shell-backend` untestable on a machine that has one.
        if let Some(name) = file.shell_backend.as_deref() {
            if !self.shell_backend_from_flag {
                self.shell_backend = crate::shell_backend::choose(None, Some(name));
            }
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
            // Keep the KDE manager's advertised default in step with the
            // per-surface answer in handlers/xdg_shell.rs::decoration_mode:
            // a client that probes the manager to decide whether to draw a
            // frame and a client that asks per-surface must get the same
            // answer, or one draws nothing while the other also draws nothing.
            use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode as KdeDefaultMode;
            self.kde_decoration_state
                .set_default_mode(if self.server_decorations {
                    KdeDefaultMode::Server
                } else {
                    KdeDefaultMode::Client
                });
        }
        // What a notification with no sound hints of its own plays. Set on
        // every load rather than only when it changed, and unconditionally
        // rather than behind a `!= default()` guard: absence here means
        // silence, so a reload that removed the key has to reach the server
        // thread as `None` or the old sound outlives the configuration that
        // asked for it.
        self.notifications
            .set_default_sound(crate::sound::Sound::from_config(
                file.notifications.sound_file.as_deref(),
                file.notifications.sound_name.as_deref(),
            ));

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
        // Only when one of those two moved. Rebuilding on any change to the
        // block would throw the loaded images away because a reload touched
        // the hide deadline, which has nothing to do with what they look like.
        if file.cursor.theme.is_some() || file.cursor.size.is_some() {
            self.cursor_theme = crate::cursor::Theme::new();
        }
        if self.cursor_hide.set_after_ms(file.cursor.hide_after_ms) {
            // The deadline that hid it has just been taken away, so nothing
            // else would ever bring it back.
            self.needs_render = true;
        }
        // A file that turned it on gets a countdown without waiting for the
        // pointer to move first, which for a desk nobody is at is never.
        self.arm_cursor_hide();

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

        // Which terminal the wallpaper is, resolved against the same
        // `terminal` the keymap uses so `true` means the one Mod4+Return
        // already opens.
        //
        // Only when the file says something: a reload that leaves the key out
        // must not take down a wallpaper a flag asked for, which is the rule
        // every other key here follows. Nothing is started or stopped from
        // here either — `start_background_process` does that once the outputs
        // exist, and a config reload cannot spawn a process behind the
        // desktop.
        self.terminal = terminal.clone();
        if file.background_terminal.is_some() {
            self.background_command =
                crate::background::resolve(file.background_terminal.as_ref(), &terminal);
            self.config.background_terminal = self.background_command.is_some();
        }

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
        crate::binding::guarantee_an_exit(&mut bindings);
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
        for surface in self.shell_client_surfaces() {
            with_surfaces_surface_tree(&surface, &release);
        }
        // And the wallpaper terminal, which is in none of the four collections
        // above and is as entitled to be paced as anything else that paints.
        //
        // Leaving it out is a client that paints its swapchain full and then
        // stops for ever. rio does exactly that: mesa's Vulkan WSI paces on
        // wp-fifo, three buffers went out in the first thirty milliseconds,
        // the fourth commit blocked on a barrier nothing here ever signalled,
        // and what was on screen was a terminal's first blank frame. It looked
        // precisely like the wallpaper not being drawn at all — which is what
        // it was reported as — and foot hid it, because foot paints into
        // shared memory and asks for no pacing.
        for surface in self.background_surfaces() {
            with_surfaces_surface_tree(&surface, &release);
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
            // And the wallpaper terminal, which is not in the space.
            //
            // Without it the clock stops under a blocked wallpaper: nothing
            // else on an otherwise empty desktop is waiting, so the tick
            // decides there is nothing to keep running for and the one client
            // that needed the next round never gets it.
            for surface in self.background_surfaces() {
                smithay::desktop::utils::with_surfaces_surface_tree(&surface, &mut look);
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
    ///
    /// `hits` is the subset of them that takes the pointer. Everything the
    /// shell floats does, bar one: see `shell_overlay_hits`.
    pub fn set_shell_overlays(
        &mut self,
        rects: Vec<smithay::utils::Rectangle<i32, Logical>>,
        hits: Vec<smithay::utils::Rectangle<i32, Logical>>,
    ) {
        if self.shell_overlays == rects && self.shell_overlay_hits == hits {
            return;
        }
        self.shell_overlay_hits = hits;
        // What is under the pointer just changed without the pointer moving,
        // and the pointer's focus is only worked out when it moves.
        //
        // This is what made notifications unclickable. One appears over a
        // window, under a pointer that is sitting still; the compositor knows
        // the click belongs to the shell — `surface_under` checks the overlays
        // first — but the *pointer* still has the window underneath as its
        // focus, and a button event goes to the focus rather than to a fresh
        // hit test. The click landed on whatever the notification was covering.
        //
        // The in-process backend never showed this: there, a click over an
        // overlay was routed by re-running the hit test at button time, since
        // the shell was not a surface and could not be a pointer focus. Moving
        // the shell into a client is what turned "checked on every click" into
        // "checked on every motion", and this is the half that went missing.
        self.refresh_pointer_focus();
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
        self.notify_output_layout_to(None);
    }

    /// The same, as one page sees it.
    ///
    /// `region` names a page's rectangle: only the screens it covers are
    /// listed, and their positions are relative to its own top-left. That is
    /// the layout the page can act on — it draws in its own document, and a
    /// screen it does not cover is one it must not place a window on.
    ///
    /// `None` is the layout as it really is, which is what a script on the
    /// control socket asked for and what a desktop spanning every screen sees
    /// anyway.
    fn output_infos(&self, region: Option<Rectangle<i32, Logical>>) -> Vec<OutputInfo> {
        let origin = region.map(|region| region.loc).unwrap_or_default();
        self.space
            .outputs()
            .filter(
                |output| match (region, self.space.output_geometry(output)) {
                    (Some(region), Some(geometry)) => geometry.overlaps(region),
                    _ => true,
                },
            )
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
                    x: geometry.loc.x - origin.x,
                    y: geometry.loc.y - origin.y,
                    width: geometry.size.w,
                    height: geometry.size.h,
                    // What is left after exclusive zones. A bar that reserved
                    // the top of the screen has taken that space away from the
                    // shell, which is the only thing that places windows.
                    usable_x: usable.loc.x - origin.x,
                    usable_y: usable.loc.y - origin.y,
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
            .collect()
    }

    /// Tell everything that lays windows out what the screens are.
    ///
    /// `only` narrows it to one page, for a page that has just started and has
    /// been told nothing yet. `None` tells all of them, and everything else on
    /// the socket.
    pub fn notify_output_layout_to(&mut self, only: Option<usize>) {
        // The shell is one buffer across the whole layout, so a change to the
        // layout is a change to its size. Without this it keeps whatever size
        // it had when it started: a monitor plugged in later, or a nested
        // window resized, leaves the rest of the screen on the clear colour.
        #[cfg(feature = "wpe")]
        self.resize_shell();
        // The same thing for the shell that is a client: it is configured to
        // the layout rather than to an output, so the layout changing is the
        // only thing that resizes it. And a monitor that was just plugged in
        // is an output the shell has not entered.
        //
        // A monitor arriving or leaving can change how many pages there are,
        // not only how big they are: a `--url` session on one screen runs the
        // page as the desktop, and the same session on two runs the page on the
        // first and the shipped desktop on the second. This starts and stops
        // them to match, and configures whatever is left running.
        self.sync_shell_processes();
        self.configure_client_shell();
        self.announce_shell_outputs();
        // And the wallpaper terminals. Unlike the shell there is one per
        // monitor, so a layout change is three things: a screen that has gone
        // takes its terminal with it, a screen that has arrived gets one, and
        // a screen that changed mode has to be told its new size.
        self.prune_background_terminals();
        self.start_background_process();
        self.configure_background();
        self.announce_background_outputs();

        // Each page hears about its own screens, in its own coordinates.
        //
        // A desktop confined to the second monitor must not be told the first
        // one exists: it would lay a window out on a screen it does not cover,
        // and the window would land on top of whatever page does cover it. And
        // the positions have to be the page's own, because that is the only
        // frame of reference its document has.
        let regions: Vec<(usize, Rectangle<i32, Logical>)> = self
            .shell_clients
            .iter()
            .enumerate()
            .filter(|(at, _)| only.is_none_or(|only| only == *at))
            .map(|(at, shell)| (at, shell.region))
            .collect();
        for (at, region) in regions {
            let outputs = self.output_infos(Some(region));
            let Some(pid) = self.shell_clients.get(at).and_then(|shell| shell.pid()) else {
                continue;
            };
            tracing::debug!(
                "shell {at}: its screens are {}",
                outputs
                    .iter()
                    .map(|o| format!("{} {}x{}{:+}{:+}", o.name, o.width, o.height, o.x, o.y))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let event = Event::OutputLayout { outputs };
            let Some(client) = self.ipc.client_for_pid(pid) else {
                // It has not connected to the control socket yet. The one it
                // makes on startup asks for this itself; see `ipc_dispatch`.
                continue;
            };
            self.ipc.send_to(client, &event);
        }
        #[cfg(feature = "wpe")]
        {
            let regions: Vec<(usize, Rectangle<i32, Logical>)> = self
                .shells
                .iter()
                .enumerate()
                .filter(|(at, _)| only.is_none_or(|only| only == *at))
                .map(|(at, page)| (at, page.region))
                .collect();
            for (at, region) in regions {
                let event = Event::OutputLayout {
                    outputs: self.output_infos(Some(region)),
                };
                if let Some(page) = self.shells.get(at) {
                    if let Err(e) = page.engine.post(&event) {
                        tracing::warn!("could not post the output layout to shell {at}: {e:#}");
                    }
                }
            }
        }

        if only.is_some() {
            return;
        }
        // And the layout as it really is, to everything else on the socket: a
        // script has no page to speak in and asked about the machine.
        let event = Event::OutputLayout {
            outputs: self.output_infos(None),
        };
        let shells: Vec<i32> = self
            .shell_clients
            .iter()
            .filter_map(|shell| shell.pid())
            .collect();
        self.ipc.broadcast_except(&shells, &event);
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
        for page in &self.shells {
            // Both directions, because a message that is sent and one that
            // arrives look the same from here and only one of them explains a
            // shell that draws its wallpaper and nothing else.
            tracing::debug!("to shell: {event:?}");
            if let Err(e) = page.engine.post(event) {
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
        // The out-of-process shell. It is not in the space and not in a layer
        // map, so nothing above reaches it — and a client that paints only when
        // invited and is never invited is a desktop that draws one frame and
        // stops.
        //
        // Every page, not only the desktop: a `--url` page on the first monitor
        // is as much a client waiting to be told to draw as the desktop on the
        // second.
        for surface in self.shell_client_surfaces() {
            smithay::desktop::utils::send_frames_surface_tree(
                &surface,
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
    pub(crate) fn create_tick(
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
    pub(crate) fn arm_tick(
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
        let mut in_space = false;
        for window in self.space.elements() {
            let active = focused.as_ref() == Some(window);
            window.set_activated(active);
            if active {
                in_space = true;
            }
        }
        // A window focused before it is mapped — a launch, where the shell's
        // `view.focus` goes out before the `view.layout` that maps it into the
        // Space — is not in the space yet, so the loop above never reaches it
        // and its client is never told it is activated. Set the state on the
        // window directly; it sits in the pending configure and goes out with
        // the window's first layout, or with any later configure.
        if !in_space {
            if let Some(window) = focused {
                window.set_activated(true);
            }
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
    /// Set on the one connection the compositor made for the shell process it
    /// started itself.
    ///
    /// This is what makes the out-of-process shell unforgeable. Recognising it
    /// by `app_id` would mean any client that named itself `dev.viewport.shell`
    /// could take the desktop's place — draw under every window, receive every
    /// click that misses one — and an `app_id` is a string a client chooses.
    /// A connection is not: this one was handed to a process the compositor
    /// spawned, over a socket pair nothing else has an end of.
    pub shell: bool,
    /// Which of them, when there is more than one.
    ///
    /// `--url` on a multi-monitor session runs two pages — the one asked for
    /// and the desktop — and both are shells by the test above. Their
    /// connections are what tells them apart, for the same reason the flag
    /// above is a connection rather than an `app_id`.
    pub shell_id: Option<u32>,
    /// Set on the one connection made for the wallpaper terminal, and
    /// unforgeable for the same reason `shell` is.
    ///
    /// What it buys is the opposite of what `shell` buys: it takes capability
    /// away rather than granting it. A client carrying this is never made a
    /// view, never enters the `Space` and is never a focus target, so nothing
    /// typed or clicked can reach it. See `crate::background`.
    pub background: bool,
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
        // A renderer of the compositor's own, on the render node, for copying
        // WebKit's frames into buffers it owns.
        //
        // Not the backend's: the copy is about owning the buffer rather than
        // about the output, and nesting under another compositor has no DRM
        // renderer at all. Both backends then import the copy into whatever
        // they draw with, which is what lets the nested one show the desktop.
        // The copy is not optional: `shell_owned` is what the compositor draws,
        // and without it the shell is absent from every frame. What is
        // best-effort is which renderer performs it — Vulkan on this GPU where
        // there is one, OpenGL on this GPU otherwise. Releasing WebKit's buffer
        // back to it depends on the copy having happened, so there is no
        // "skip the copy and show the buffer" to fall back to: the engine
        // paints into the picture on screen, and the alternative to that is
        // holding the buffer, which deadlocks the engine after one frame.
        if self.shell_renderer.is_none() && !self.shell_copy_refused {
            let make = || -> anyhow::Result<(
                crate::udev::Gpu,
                smithay::backend::allocator::gbm::GbmAllocator<smithay::backend::drm::DrmDeviceFd>,
            )> {
                // VIEWPORT_RENDERER=gles means this renderer too. It steered
                // only the outputs before, which left a session forced onto
                // OpenGL still copying the shell's frames with Vulkan — the one
                // renderer the switch exists to take out of the picture, and
                // the one whose failure to import the copy is the reason to
                // reach for the switch at all.
                let forced_gles = crate::udev::renderer_forced_gles();
                let device = if forced_gles {
                    Err(anyhow::anyhow!("VIEWPORT_RENDERER asked for OpenGL"))
                } else {
                    let instance = smithay::backend::vulkan::Instance::new(
                        smithay::backend::vulkan::version::Version::VERSION_1_3,
                        None,
                    )
                    .map_err(|e| {
                        anyhow::anyhow!("creating a vulkan instance for the shell: {e}")
                    })?;
                    // The device borrows nothing from the instance: Smithay's
                    // `PhysicalDevice` holds its own handle to it, which is what
                    // lets the instance be built inside this branch.
                    viewport_vulkan::Device::for_node_exactly(&instance, render)
                        .map_err(|e| anyhow::anyhow!("opening a vulkan device for the shell: {e}"))
                };
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
                // Through `DrmDeviceFd` rather than holding the `File`: it is
                // an `Arc` around the descriptor and so clones, and both the
                // GBM device and the allocator taken from it have to.
                let fd = smithay::backend::drm::DrmDeviceFd::new(smithay::utils::DeviceFd::from(
                    std::os::fd::OwnedFd::from(file),
                ));
                let gbm = smithay::backend::allocator::gbm::GbmDevice::new(fd)
                    .map_err(|e| anyhow::anyhow!("creating a gbm device for the shell: {e}"))?;
                let allocator = smithay::backend::allocator::gbm::GbmAllocator::new(
                    gbm.clone(),
                    smithay::backend::allocator::gbm::GbmBufferFlags::RENDERING,
                );
                // Vulkan on *this* device or OpenGL on this device — never
                // Vulkan on some other one. `for_node_exactly` is the whole
                // difference: the loose `for_node` falls back to any Vulkan
                // device there is, and in a virtual machine that is lavapipe,
                // which owns no DRM node and cannot see these buffers.
                let renderer = match device {
                    Ok(device) => viewport_vulkan::VulkanRenderer::with_allocator(
                        &device,
                        allocator.clone(),
                    )
                    .map(crate::udev::Gpu::Vulkan)
                    .map_err(|e| anyhow::anyhow!("creating a vulkan renderer: {e}"))?,
                    Err(_) if forced_gles => {
                        tracing::info!(
                            "VIEWPORT_RENDERER: copying the shell's frames with OpenGL too"
                        );
                        crate::udev::Gpu::Gles(Box::new(crate::udev::gles_renderer(&gbm)?))
                    }
                    Err(e) => {
                        tracing::info!(
                            "no Vulkan on the shell's GPU ({e:#}); copying its frames with OpenGL"
                        );
                        crate::udev::Gpu::Gles(Box::new(crate::udev::gles_renderer(&gbm)?))
                    }
                };
                Ok((renderer, allocator))
            };
            match make() {
                Ok((renderer, allocator)) => {
                    self.shell_renderer = Some(renderer);
                    self.shell_allocator = Some(allocator);
                }
                Err(e) => {
                    tracing::warn!(
                        "no renderer to copy the shell's frame with ({e:#}); \
                         the shell will not be drawn"
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

        let console = std::env::var("VIEWPORT_LOG")
            .map(|level| level.contains("debug") || level.contains("trace"))
            .unwrap_or(false);

        let size = self.layout_size();
        anyhow::ensure!(
            size.0 > 0 && size.1 > 0,
            "the shell needs an output to size itself against"
        );

        // Which pages, where, and which of them runs the desktop. The same
        // decision the out-of-process backend makes, from the same function:
        // `--url` on a session with more than one monitor is that page on the
        // first screen and the shipped shell on the rest. See
        // `shell_client::plan_shells`.
        let planned = self.plan_shells();
        let mut started = Vec::with_capacity(planned.len());
        for plan in planned {
            let size = (
                plan.region.size.w.max(0) as u32,
                plan.region.size.h.max(0) as u32,
            );
            if size.0 == 0 || size.1 == 0 {
                tracing::warn!("not starting {}: it was given no room", plan.url);
                continue;
            }
            tracing::info!(
                "starting the shell at {}, {}x{}{}",
                plan.url,
                size.0,
                size.1,
                if plan.desktop { "" } else { " (page only)" }
            );
            let engine = crate::shell::Shell::start(
                &card_path,
                &render_path,
                &formats,
                size,
                &plan.url,
                console,
            )?;
            if let Some(ping) = self.shell_ping.clone() {
                engine.wake_with(ping);
            }
            started.push(crate::shell::Page {
                engine,
                url: plan.url,
                region: plan.region,
                desktop: plan.desktop,
                size: Some(size),
                owned: None,
                damage: Default::default(),
                element_id: smithay::backend::renderer::element::Id::new(),
                restarts: 0,
                restart_window: None,
                announced: false,
            });
        }
        self.shells = started;
        Ok(())
    }
}

#[cfg(feature = "wpe")]
impl ViewportState {
    /// The modifiers the shell's copy buffer may be allocated with.
    ///
    /// The intersection of what the copy renderer can draw into and what the
    /// renderer that draws the desktop can sample from, because the buffer is
    /// handed from one to the other. They are usually the same device and
    /// usually the same renderer, but not always: the copy runs on the render
    /// node's own renderer, and under a nested backend the desktop is drawn by
    /// a renderer that never saw that node.
    ///
    /// Empty when there is nothing in common — or when neither advertises a
    /// modifier at all, which is an OpenGL driver without the modifier
    /// extensions. [`crate::dump::owned_image`] allocates implicitly then,
    /// which is what such a driver wants.
    fn shell_copy_modifiers(&self) -> Vec<smithay::backend::allocator::Modifier> {
        let Some(copy) = self.shell_renderer.as_ref() else {
            return Vec::new();
        };
        let importable = |formats: smithay::backend::allocator::format::FormatSet| {
            formats
                .iter()
                .filter(|format| format.code == smithay::backend::allocator::Fourcc::Argb8888)
                .map(|format| format.modifier)
                .collect::<Vec<_>>()
        };
        let mine = importable(copy.dmabuf_formats());
        let Some(theirs) = self
            .udev
            .as_ref()
            .map(|udev| importable(udev.primary().renderer.dmabuf_formats()))
        else {
            // No DRM renderer to hand it to: nested, where the backend's own
            // renderer imports it and the copy renderer's set is the only one
            // this side of the compositor knows.
            return mine;
        };
        let both: Vec<_> = mine
            .iter()
            .copied()
            .filter(|modifier| theirs.contains(modifier))
            .collect();
        if both.is_empty() && !mine.is_empty() {
            // Two renderers on one GPU with no ARGB8888 modifier in common
            // should not happen, and if it does the buffer cannot both be
            // drawn into and be sampled from whatever it is allocated as. The
            // copy renderer wins, because a copy that fails is a shell that is
            // never drawn at all, while an import that fails says so per
            // output and names the modifier.
            tracing::warn!(
                "the shell's copy renderer and the display's share no ARGB8888 modifier; \
                 allocating for the copy"
            );
            return mine;
        }
        both
    }

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
        // By index, because the copy below hands the renderer back out of
        // `self` and then reaches into it again: a borrow of one page held
        // across that is a borrow of the whole compositor.
        for at in 0..self.shells.len() {
            self.import_page_frame(at);
        }
    }

    /// The same, for one page.
    fn import_page_frame(&mut self, page: usize) {
        use smithay::backend::allocator::Buffer as _;
        use smithay::backend::renderer::ImportDma as _;

        if let Some(pending) = self.shells[page].engine.take_frame() {
            let size: smithay::utils::Size<i32, smithay::utils::Physical> = (
                pending.buffer.width() as i32,
                pending.buffer.height() as i32,
            )
                .into();
            let first = self.shells[page].owned.is_none();

            // The whole buffer, because WebKit's per-frame damage rectangles
            // are not carried across the shim. Redrawing more than changed
            // costs a composite; reporting none at all stops the output.
            self.shells[page]
                .damage
                .add([smithay::utils::Rectangle::from_size(
                    size.to_logical(1)
                        .to_buffer(1, smithay::utils::Transform::Normal),
                )]);

            // Allocated before the old one is given up, not after.
            //
            // The old buffer is the picture on screen. Taking it first and then
            // failing to replace it — the layout changed and the device is out
            // of memory, or the renderer is gone — drops the shell out of the
            // render list entirely, which is a grey half of a desktop that
            // comes back only if WebKit paints again. Holding a stale frame is
            // the better failure: it is wrong by one layout, not absent.
            let stale = match self.shells[page].owned.as_ref() {
                Some((_, at)) => *at != size,
                // First frame.
                None => true,
            };
            if stale {
                // Two renderers touch this buffer: the shell's copies into it,
                // and the output's samples from it. Only a modifier both of
                // them advertise works, and on a machine where one is Vulkan
                // that rules out the implicit one entirely — see `owned_image`.
                let modifiers = self.shell_copy_modifiers();
                match self
                    .shell_allocator
                    .as_mut()
                    .map(|allocator| crate::dump::owned_image(allocator, size, &modifiers))
                {
                    Some(Ok(buffer)) => self.shells[page].owned = Some((buffer, size)),
                    Some(Err(e)) => tracing::error!(
                        "could not allocate a {}x{} image for the shell's frame: {e:#}",
                        size.w,
                        size.h
                    ),
                    None => tracing::error!("no allocator for the shell's frame"),
                }
            }

            // Import and copy in one place, because the texture belongs to the
            // renderer that made it: a Vulkan texture and a GLES texture share
            // a trait and nothing else, so the copy has to happen while that
            // renderer is still in hand. Taken out of `self` for the duration
            // so the body can reach the rest of it.
            let mut renderer = self.shell_renderer.take();
            if let Some(gpu) = renderer.as_mut() {
                crate::with_gpu!(gpu, |shell_renderer| {
                    match shell_renderer.import_dmabuf(&pending.buffer, None) {
                        Ok(texture) => {
                            // Once. "The shell did not appear" has two causes
                            // that look identical in the log otherwise: WebKit
                            // never painted, or it painted and the frame was
                            // not drawn.
                            if first {
                                tracing::info!(
                                    "shell {page}: first frame imported, {}x{}",
                                    size.w,
                                    size.h
                                );
                            }
                            match self.shells[page].owned.take() {
                                // Only into a buffer the frame actually fits.
                                // The allocation above failed if this does not
                                // match, and copying anyway would paint a new
                                // frame into part of an old one — a torn
                                // composite of two layouts, which reads as a
                                // rendering bug rather than as the allocation
                                // failure it is.
                                Some((mut buffer, at)) if at == size => {
                                    if let Err(e) = crate::dump::copy_texture(
                                        shell_renderer,
                                        &texture,
                                        &mut buffer,
                                        at,
                                    ) {
                                        tracing::error!("could not copy the shell's frame: {e:#}");
                                    }
                                    // Whichever renderer draws this output
                                    // imports it itself — see `render::build`.
                                    self.shells[page].owned = Some((buffer, at));
                                }
                                Some(kept) => {
                                    tracing::warn!(
                                        "keeping the shell's last frame; this one has nowhere to go"
                                    );
                                    self.shells[page].owned = Some(kept);
                                }
                                None => {
                                    tracing::error!("no image to copy the shell's frame into")
                                }
                            }
                        }
                        Err(e) => tracing::error!("could not import the shell's frame: {e}"),
                    }
                });
            }

            // What WebKit actually painted, once, before anything else can
            // have touched it — the one thing the log cannot say, and the
            // difference between an empty right half and a right half put on
            // screen wrongly. Vulkan only: it is a diagnostic for the renderer
            // that has colour management, and teaching it a second one buys
            // nothing. Re-imported rather than threaded out of the body above,
            // because it runs on the first frame of a session that asked for
            // it and nowhere else.
            if first {
                if let (Some(path), Some(crate::udev::Gpu::Vulkan(vulkan))) =
                    (crate::dump::target(), renderer.as_mut())
                {
                    match vulkan.import_dmabuf(&pending.buffer, None) {
                        Ok(texture) => {
                            if let Err(e) = crate::dump::shell_frame(vulkan, &texture, &path) {
                                tracing::error!("could not dump the shell's frame: {e:#}");
                            }
                        }
                        Err(e) => tracing::error!("could not import for the dump: {e}"),
                    }
                }
            }
            self.shell_renderer = renderer;

            {
                let shell = &self.shells[page].engine;
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
            }
            self.shell_frames += 1;
            tracing::debug!("shell frame {} released", self.shell_frames);
        }

        let shell = &self.shells[page].engine;
        // Frames the mailbox threw away before anything drew them.
        for token in shell.take_stale() {
            shell.frame_release(token);
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
    pub fn restart_shell(&mut self, page: usize, reason: viewport_web::webkit::Termination) {
        use crate::shell::Recovery;

        if self.shells.get(page).is_none() {
            return;
        }
        if !reason.is_recoverable() {
            tracing::warn!("not restarting shell {page}: {reason}");
            return;
        }

        // Per page, not per session. One page crashing says nothing about the
        // health of the other, and a shared budget would let a site that
        // reloads badly use up the desktop's attempts.
        let attempt = {
            let shell = &mut self.shells[page];
            crate::shell::budget(
                &mut shell.restarts,
                &mut shell.restart_window,
                std::time::Instant::now(),
            )
        };

        let attempt = match attempt {
            Recovery::Restart(attempt) => attempt,
            Recovery::GiveUp(count) => {
                // The page is gone either way; what stopping preserves is a
                // machine that can still be logged into and read the log.
                tracing::error!(
                    "shell {page} has died {count} times in {:?}; giving up",
                    crate::shell::RESTART_WINDOW
                );
                // Dropping the copy takes the page out of the element list,
                // and the damage tracker repaints what it covered because the
                // element it knew is gone. Nothing has to be added to its
                // damage bag: that is only read while there is a buffer to
                // describe.
                self.shells[page].owned = None;
                self.needs_render = true;
                return;
            }
        };

        tracing::warn!("restarting shell {page} after {reason} (attempt {attempt})");

        // The new process is a fresh page: it has painted nothing, said
        // nothing, and knows nothing about the layout. Everything derived from
        // the old one has to go with it, or the log claims a shell that is
        // talking and painting while the screen shows neither.
        self.shell_frames = 0;
        self.shell_announced = false;
        self.shells[page].announced = false;
        self.shells[page].size = None;

        match self.shells[page].engine.restart() {
            // Unconditionally, because the size was just cleared: WebKit paints
            // nothing into a view of no size, and a restarted process that is
            // never told its size loads the page and then sits there.
            Ok(()) => self.resize_shell(),
            Err(e) => tracing::error!("could not restart shell {page}: {e:#}"),
        }
    }

    /// Tell the shell how big it is.
    ///
    /// WebKit paints nothing into a view with no size, so without this the
    /// page loads, runs, talks to the compositor — and never produces a frame.
    pub fn resize_shell(&mut self) {
        let (width, height) = self.layout_size();
        if width == 0 || height == 0 {
            return;
        }
        // What the screens now imply, which for a `--url` session can be a
        // different *number* of pages as well as different sizes — see
        // `sync_shells`.
        self.sync_shells();

        for at in 0..self.shells.len() {
            let size = {
                let region = self.shells[at].region;
                (region.size.w.max(0) as u32, region.size.h.max(0) as u32)
            };
            if size.0 == 0 || size.1 == 0 {
                continue;
            }
            // Only on a change: this is called from notify_output_layout, which
            // runs for anything that touches the layout — including a layer
            // surface arriving — and telling WebKit to resize to the size it
            // already has costs a full repaint.
            if self.shells[at].size == Some(size) {
                continue;
            }
            self.shells[at].size = Some(size);
            tracing::info!(
                "shell {at} is {}x{} now, for {}",
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
            self.shells[at].engine.resize(size.0, size.1);
        }
    }

    /// Start or stop pages so that what is running matches what the screens
    /// call for, and move the ones that stay.
    ///
    /// The out-of-process twin of this is `sync_shell_processes`, and the rule
    /// is the same: reconcile by the document each page is showing rather than
    /// rebuilding, so plugging a monitor into a `--url` session resizes the
    /// page that was already up instead of reloading it.
    ///
    /// A page that has to be *started* here cannot be: `Shell::start` needs the
    /// DRM nodes and the importable format list, which live in the backend and
    /// not in this state. So a plan that calls for one is reported and the
    /// pages that exist are placed as well as they can be — the desktop keeps
    /// the whole layout, which is what it had before the monitor arrived.
    fn sync_shells(&mut self) {
        if self.shells.is_empty() {
            return;
        }
        let planned = self.plan_shells();
        if planned.len() != self.shells.len() {
            tracing::warn!(
                "the screens now call for {} page(s) and {} are running; the in-process engine \
                 cannot start one after the session has begun, so the layout is unchanged",
                planned.len(),
                self.shells.len()
            );
            return;
        }
        for (page, plan) in self.shells.iter_mut().zip(planned) {
            if page.url != plan.url {
                // The plan is positional and both entries are running, so this
                // is the page and the desktop having swapped places, which
                // nothing produces today.
                tracing::warn!("shell plan changed under a running page; leaving it where it is");
                continue;
            }
            page.region = plan.region;
            page.desktop = plan.desktop;
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
/// What a pointer drag is doing.
///
/// Three gestures on one mechanism, because all three need the same thing: a
/// button held down, deltas while it is, and a grab that survives the pointer
/// crossing onto a client — the drag belongs to whoever started it, wherever
/// the hand goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    /// Left button on a window: move it.
    Move,
    /// Right button on a window: resize it.
    Resize,
    /// Left button on the desktop, where there is no window at all. Nothing to
    /// move, so it moves the view instead — which is only meaningful to a
    /// layout that has one, and is dropped by every layout that does not.
    Pan,
}

pub struct PointerDrag {
    /// The window being dragged. Meaningless for [`DragKind::Pan`], which is
    /// about the desktop rather than about anything on it.
    pub id: u32,
    /// The button held down, which is the one whose release ends this drag.
    ///
    /// Kept because the other one can be pressed and released in the middle of
    /// a drag without meaning anything to it, and a drag that ended on
    /// whichever button came up next let go of the window while it was still
    /// being held.
    pub button: u32,
    /// Which of the three gestures this is.
    pub kind: DragKind,
    /// For a resize, which corner it took hold of, as `(west, north)`: whether
    /// the left edge moves and whether the top one does. `(false, false)` — the
    /// bottom right, which is what every resize used to be — for the other two
    /// gestures, which move no edges at all.
    ///
    /// Settled at the press and kept for the whole drag: the pointer crossing
    /// the middle of the window mid-drag is not a change of mind about which
    /// corner is in the hand.
    pub edges: (bool, bool),
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

/// A point on screen, in the coordinates of something drawn `scale` smaller
/// about `origin`.
///
/// The inverse of what `RescaleRenderElement` does in render.rs, and the whole
/// of what a hit test on a shrunken window needs: the client was never resized,
/// so the point it should be asked about is the one it would have been at if
/// the shrinking had not happened.
///
/// A free function so that the arithmetic — the part that is wrong in a way
/// nobody can see until they click — is checkable without a `Space`, an output
/// or a client.
fn unscale_about(
    origin: Point<f64, Logical>,
    pos: Point<f64, Logical>,
    scale: f64,
) -> Point<f64, Logical> {
    (
        origin.x + (pos.x - origin.x) / scale,
        origin.y + (pos.y - origin.y) / scale,
    )
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f64, y: f64) -> Point<f64, Logical> {
        (x, y).into()
    }

    /// The corner a window is scaled about does not move, whatever the scale.
    /// Everything else is measured from it.
    #[test]
    fn the_corner_is_where_the_scaling_happens() {
        let origin = at(100.0, 50.0);
        for scale in [0.25, 0.5, 1.0] {
            assert_eq!(unscale_about(origin, origin, scale), origin, "at {scale}");
        }
    }

    /// Half size: a point twenty pixels from the corner on screen is forty
    /// pixels into the client, because the client was never made smaller — it
    /// is drawn smaller.
    #[test]
    fn a_point_on_a_shrunken_window_is_further_into_the_client() {
        let origin = at(100.0, 50.0);
        let hit = unscale_about(origin, at(120.0, 70.0), 0.5);
        assert_eq!(hit, at(140.0, 90.0));

        // And the error grows with the distance, which is why this shows up as
        // "the left of the window works and the right of it does not" rather
        // than as a constant offset anybody would spot at once.
        let near = unscale_about(origin, at(102.0, 52.0), 0.5);
        let far = unscale_about(origin, at(500.0, 450.0), 0.5);
        assert_eq!(near, at(104.0, 54.0));
        assert_eq!(far, at(900.0, 850.0));
    }

    /// Full size changes nothing at all — every window in every layout that
    /// does not shrink one.
    #[test]
    fn an_unscaled_window_is_left_alone() {
        let origin = at(100.0, 50.0);
        for point in [at(0.0, 0.0), at(100.0, 50.0), at(1920.0, 1080.0)] {
            assert_eq!(unscale_about(origin, point, 1.0), point);
        }
    }

    /// What `surface_under` hands back is not the surface's origin — it is
    /// whatever makes the pointer's own subtraction come out right.
    ///
    /// The pointer computes `pointer_position - returned` and gives the client
    /// the result. The pointer position is in screen coordinates and the client
    /// thinks in its own, so returning the surface's actual origin is a window
    /// that is *found* correctly and then told a coordinate off by the whole
    /// scale error — zero at its corner and growing across it. This pins the
    /// relationship rather than the value, which is the thing that has to hold.
    #[test]
    fn the_pointer_is_handed_the_clients_own_coordinate() {
        let origin = at(200.0, 100.0);
        // Where the surface starts, and where the found subsurface sits inside
        // it. Both in the window's own coordinates, as Smithay reports them.
        let render_location = at(180.0, 80.0);
        let inner = at(12.0, 8.0);

        for scale in [1.0, 0.5, 0.25] {
            // A point on the screen, and the same point in the window's own
            // coordinates.
            let pos = at(640.0, 400.0);
            let unscaled = unscale_about(origin, pos, scale);

            // What the compositor returns, and what the pointer then works out.
            let local = at(
                unscaled.x - render_location.x - inner.x,
                unscaled.y - render_location.y - inner.y,
            );
            let returned = at(pos.x - local.x, pos.y - local.y);
            let told = at(pos.x - returned.x, pos.y - returned.y);

            assert!(
                (told.x - local.x).abs() < 1e-9 && (told.y - local.y).abs() < 1e-9,
                "at {scale}: the client is told {told:?}, not {local:?}"
            );

            // And at full size it is the plain sum it has always been, so
            // nothing changes for the layouts that never scale a window.
            if scale == 1.0 {
                assert_eq!(
                    returned,
                    at(render_location.x + inner.x, render_location.y + inner.y)
                );
            }
        }
    }

    /// Scaling out and back is where it started, which is what makes the
    /// pointer land on the pixel it is over: the renderer takes the client's
    /// point to the screen and this takes the screen's point back.
    #[test]
    fn it_is_the_inverse_of_what_the_renderer_did() {
        let origin = at(30.0, 70.0);
        for scale in [0.25, 0.4, 0.5, 0.75] {
            let client = at(640.0, 360.0);
            // What render.rs draws it at: origin + (client - origin) * scale.
            let drawn = at(
                origin.x + (client.x - origin.x) * scale,
                origin.y + (client.y - origin.y) * scale,
            );
            let back = unscale_about(origin, drawn, scale);
            assert!(
                (back.x - client.x).abs() < 1e-9 && (back.y - client.y).abs() < 1e-9,
                "at {scale}: {back:?} is not {client:?}"
            );
        }
    }
}
