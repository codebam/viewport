// SPDX-License-Identifier: GPL-3.0-or-later
//
// Compositor state. Ports src/server.c.

use std::ffi::OsString;
use std::os::unix::fs::OpenOptionsExt as _;
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

/// An offscreen kept between captures, and what shape it is.
///
/// Boxed as `Any` in the state: what a renderer composites into is its own
/// type, and the capture paths are generic over it.
struct CaptureScratch<B> {
    format: smithay::backend::allocator::Fourcc,
    size: smithay::utils::Size<i32, smithay::utils::Buffer>,
    buffer: B,
}

/// How many capture buffers are held between frames.
///
/// One per thing being captured at once — a monitor, the desk, a window or two
/// — and past that the oldest goes, rather than a screen's worth of memory
/// being held for a shape nothing asks for any more.
const KEPT_CAPTURE_TARGETS: usize = 4;

/// How many shell overlay rectangles one message may carry.
///
/// Generous far past what a desktop draws — notifications, a chooser, an
/// overview's worth — and finite because the list arrives over the control
/// socket and is walked on every frame; see `set_shell_overlays`.
const MAX_SHELL_OVERLAYS: usize = 4096;

/// How long a monitor change stands before it is undone unconditionally.
///
/// Twelve seconds, which is what `docs/ipc.md` has promised since before
/// anything armed the deadline. Long enough to find the button on a screen
/// that came back looking wrong, short enough that a screen that did not come
/// back at all is not a session somebody has to reboot out of. See
/// [`ViewportState::arm_output_revert`].
pub const OUTPUT_REVERT_AFTER: std::time::Duration = std::time::Duration::from_secs(12);

/// Take back a capture buffer of this exact shape, if one is held.
fn take_scratch<B: 'static>(
    held: &mut Vec<Box<dyn std::any::Any>>,
    format: smithay::backend::allocator::Fourcc,
    size: smithay::utils::Size<i32, smithay::utils::Buffer>,
) -> Option<B> {
    let at = held.iter().position(|entry| {
        entry
            .downcast_ref::<CaptureScratch<B>>()
            .is_some_and(|scratch| scratch.format == format && scratch.size == size)
    })?;
    // Cannot fail: the position was found by downcasting to this very type.
    let scratch = held.remove(at).downcast::<CaptureScratch<B>>().ok()?;
    Some(scratch.buffer)
}

/// Hold a capture buffer for the next capture of the same shape.
fn keep_scratch<B: 'static>(
    held: &mut Vec<Box<dyn std::any::Any>>,
    format: smithay::backend::allocator::Fourcc,
    size: smithay::utils::Size<i32, smithay::utils::Buffer>,
    buffer: B,
) {
    held.push(Box::new(CaptureScratch {
        format,
        size,
        buffer,
    }));
    while held.len() > KEPT_CAPTURE_TARGETS {
        held.remove(0);
    }
}

/// Where a portal screenshot is written on its way to whoever asked.
///
/// Under `$XDG_RUNTIME_DIR` — the runtime directory of the user this
/// compositor is running as, 0700 on every distribution that follows the spec
/// — rather than `/tmp`: the old name was predictable from the pid and the
/// clock, and `/tmp` is walkable by everyone, which together made any local
/// user able to pre-plant or pre-read another's screenshot. Inside the runtime
/// directory nobody else can walk at all. The file is created fail-if-exists
/// and born 0600 besides, so even a directory that should not be shared stays
/// safe; see the writer in `service_portal_screenshots`.
///
/// A subdirectory of our own rather than the runtime directory itself, so the
/// housekeeping tick has one place to sweep and nothing else there can be
/// mistaken for litter.
fn screenshot_temp_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("viewport-screenshots");
    // Reused when it is already ours: an existing directory is not an error
    // worth reporting, and the files inside it are created private whatever
    // the directory turned out to be.
    use std::os::unix::fs::DirBuilderExt as _;
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir);
    dir.join(format!(
        "viewport-screenshot-{}-{}.png",
        std::process::id(),
        // Nanos, not millis: the file is created fail-if-exists, and two
        // screenshots asked for within the same millisecond used to be a
        // collision one of them lost.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ))
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

/// What starting a share put in motion, and what its answer is made of.
///
/// The stream itself is already in `casts`; this carries everything else the
/// portal reply needs, which cannot be read back later — most of it because
/// it described the source before the source was taken, and the node because
/// it does not exist yet at all. See [`ViewportState::start_cast`].
struct BegunCast {
    /// Where the naming stands, and who to tell when it moves.
    arrival: crate::screencast::stream::Arrival,
    /// Which stream in `casts` this is, for taking it back out if the node
    /// never arrives: until then every stream answers the same placeholder
    /// node number, and the wrong teardown would take somebody else's share.
    stream_id: u64,
    size: smithay::utils::Size<i32, smithay::utils::Physical>,
    /// What it is called, for the log line and for saying what was lost.
    name: String,
}

/// A share begun and not yet answered: the stream exists, but PipeWire has
/// not named its node yet, so the portal reply that carries the number is
/// still owed.
///
/// Everything the *success* needs travels with the promise instead, because
/// that is where the success leaves from; what waits here is exactly what a
/// failure needs, since giving up is done from the compositor's timer and
/// that is the only place allowed to tear a half-made stream back out. See
/// [`ViewportState::finish_share`].
struct PendingShare {
    /// Which deadline this is. Rising per share, like the chooser's id, so a
    /// timer firing late cannot settle somebody else's share.
    id: u64,
    arrival: crate::screencast::stream::Arrival,
    stream_id: u64,
    name: String,
    reply: crate::screencast::Reply,
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
    /// What was notified, kept after the popup has gone.
    ///
    /// Here rather than in the shell because the shell is a page that
    /// restarts and reloads; see `crate::notification::History`.
    pub notification_history: crate::notification::History,
    /// The system tray, forwarded to the shell the same way.
    pub tray: crate::tray::Tray,
    /// What is playing, for the bar's media widget. Idle unless one is on it.
    pub mpris: crate::mpris::Mpris,
    /// Battery, lid and power profiles. Idle unless a widget or lid policy
    /// wants it.
    pub power: crate::power::Power,
    /// The wireless radio, for the shell's network picker. Idle until one
    /// opens; the compositor has no use for it otherwise.
    pub network: crate::network::Network,
    /// The Bluetooth adapter, for the shell's Bluetooth picker. Idle on the
    /// same terms, and stops the radio scanning when the picker closes.
    pub bluetooth: crate::bluetooth::Bluetooth,
    /// What closing the lid does. Applied here rather than on the worker:
    /// lock and blank are compositor policy.
    pub lid: crate::power::LidAction,
    /// Last lid state the worker reported, so a repeat snapshot does not
    /// lock again.
    pub last_lid_closed: Option<bool>,
    /// The last few things copied, so a selection outlives the client that
    /// offered it.
    pub clipboard: crate::clipboard::Clipboard,
    /// The applications the last `launcher.query` answered with, in the order
    /// the `launcher.list` named them.
    ///
    /// Kept because `launcher.launch` names an index into that list rather
    /// than a name: an `id` from a list that is no longer on screen is refused
    /// rather than guessed at.
    pub launcher_list: Vec<crate::launcher::App>,
    /// The list `launcher_list` is, counted in queries.
    ///
    /// A `launcher.launch` carries the generation of the list its row came
    /// from, and one that names a generation the compositor has moved past is
    /// refused: the keystroke that asked for the list the row is in may still
    /// be on its way, and an `id` from the old list is almost always in range
    /// of the new one — the wrong application is worse than nothing.
    pub launcher_generation: u64,
    /// The launcher's scanner: the thread that walks the applications
    /// directories and resolves the icons, and the mailbox to reach it on.
    ///
    /// A query used to do both on this thread — a directory walk and up to a
    /// hundred icon reads per keystroke, with frames waiting behind them. Now
    /// the keystroke posts and the answer comes back through a calloop
    /// channel the way a notification does; see `crate::launcher::Scanner`,
    /// which keeps the icon cache on the thread that fills it. Absent where
    /// the thread would not start, and the query answered here as it always
    /// was.
    pub launcher_scan: crate::launcher::Scanner,
    /// The icon theme the launcher's icons are looked up in.
    ///
    /// The tray keeps its own copy for its worker thread; this one travels
    /// with each query the launcher's scanner is sent, so an answer is always
    /// drawn from the theme it was asked under. Both are set from the same
    /// key, in the same place.
    pub icon_theme: String,
    /// Whether the configuration wants a tray at all. Kept because the
    /// configuration is read before the event loop has anywhere to send one,
    /// and `Tray::attach` acts on it once it does.
    pub tray_enabled: bool,
    /// The settings portal, which is how a client learns the session is dark.
    pub appearance: crate::appearance::Appearance,
    /// System statistics for the bar, sampled here because the page cannot.
    pub status: crate::status::Status,
    /// Locking and blanking after a while, off unless the file asks.
    pub idle: crate::idle::Idle,
    pub idle_settings: crate::idle::Settings,
    /// Global shortcuts an application is allowed to hear, by session.
    ///
    /// Read on every key press that no binding claimed, which is why it is a
    /// flat list rather than a map: it is empty on a desktop where nothing has
    /// asked, and a handful of entries where something has.
    pub shortcuts: Vec<crate::shortcuts::Grant>,
    /// Which of those are held down right now, by the key that fired them, so
    /// the release can say which shortcut stopped.
    pub shortcuts_held: Vec<(u32, crate::shortcuts::Fired)>,
    /// Shortcuts that fired during the key filter, waiting to be announced.
    ///
    /// Queued rather than emitted where they are noticed: the filter runs
    /// inside the keyboard's own borrow of the seat, and a D-Bus write from
    /// there would be one more thing happening under a lock that the rest of
    /// the input path depends on.
    pub shortcuts_to_announce: Vec<(bool, crate::shortcuts::Fired)>,
    /// Whose request the chooser on screen is answering, when it is a
    /// shortcuts one. The session handle is not something the person has to
    /// read, so it travels here rather than on the `Picker` beside the chords
    /// and the application name, which are.
    pub pending_shortcuts: Option<(zvariant::OwnedObjectPath, String)>,
    /// What was agreed to in earlier sessions.
    pub shortcuts_store: crate::shortcuts::Store,
    /// How the `Activated` and `Deactivated` signals get out.
    pub shortcut_signals: crate::shortcuts::Signals,
    /// The `org.freedesktop.ScreenSaver` connection, kept because dropping it
    /// takes the interface off the bus. Nothing else reads it.
    pub screensaver: Option<zbus::blocking::Connection>,
    /// Everything holding idle off from the session bus.
    ///
    /// Separate from `idle_inhibitors`, which is the Wayland protocol's list
    /// of surfaces, because these are not surfaces and do not die with one: a
    /// browser inhibiting over `org.freedesktop.ScreenSaver` has a connection
    /// and a cookie. Both lists answer the same question and
    /// `refresh_idle_inhibit` asks both. See `crate::inhibit`.
    pub bus_inhibitors: crate::inhibit::Registry,
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
    /// The ones that have crashed and are waiting out a backoff before being
    /// started again. See `shell_client::restart_backoff`.
    pub pending_shells: Vec<crate::shell_client::PendingShell>,
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
    /// The socket client whose message is being applied, or 0 for nobody in
    /// particular.
    ///
    /// Set by `ipc_dispatch_at` around a dispatch, exactly as
    /// `dispatch_origin` is, so `apply::reject` can answer the client that
    /// sent the message instead of every client listening. Zero outside a
    /// dispatch, which is what makes the config file's own rejections —
    /// which nobody in particular asked for — broadcasts.
    pub dispatch_client: u64,
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
    /// The screen magnifier. Off until a chord turns it on, and off is what
    /// `Default` gives, so a session that never presses one pays nothing —
    /// `is_on()` is the only thing the render path asks. See
    /// [`crate::magnify`].
    pub magnifier: crate::magnify::Magnifier,
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

    /// The stack is owed a `restack`, and the colour-management clients are
    /// owed a look at whether their screen changed under them.
    ///
    /// Both used to run at the end of every `view.layout`, and the shell sends
    /// one of those per window per animation frame — so a desktop with eight
    /// windows sliding across it did the whole of `restack` and the whole of
    /// `notify_surface_colour` eight times for one frame of one animation, when
    /// the answer only has to be right once, by the time anything looks. See
    /// `settle`.
    pub needs_restack: bool,
    pub needs_colour_notify: bool,

    /// Foreign-toplevel output lists are owed for the same reason: the shell
    /// lays every window out per animation frame, and `sync_foreign_outputs`
    /// walks all of them. See `settle`.
    pub needs_foreign_outputs: bool,

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
    /// Whether the client with keyboard focus had an active text-input, last
    /// time `sync_osk_wanted` in `input.rs` checked. Mirrored to the shell as
    /// `osk.wanted` only when it changes, which is what this is kept for: the
    /// check itself is cheap enough to run on every commit, but a message the
    /// shell was not going to act on differently is not free once WebKit has
    /// to receive and parse it.
    pub osk_wanted: bool,
    /// What `osk` in the config file said: whether the keyboard may raise
    /// itself, and whether the chord still works if it may not. Read by
    /// `sync_osk_wanted` in `input.rs`; see `config::OskMode` for what each
    /// value means and why a boolean could not say it.
    pub osk_mode: crate::config::OskMode,
    /// What `xwayland.scale` in the config file said. Read exactly once, by
    /// [`Self::start_xwayland`], which runs after `apply_config` and before
    /// the event loop turns; a reload moves this and nothing else, for the
    /// reason `config::XwaylandConfig::scale` gives.
    pub xwayland_scale: crate::config::XwaylandScale,
    /// Whether a touch-capable input device has ever been seen on this seat,
    /// from `DeviceCapability::Touch` on an `InputEvent::DeviceAdded` in
    /// `input.rs`. Sticky rather than a live count: `seat.add_touch()` in
    /// `Self::new`, below, gives every seat a `wl_touch` unconditionally, so
    /// unlike a tablet this cannot be read off the seat itself, and a
    /// touchscreen unplugged mid-session should not suddenly stop the
    /// keyboard it was raising a moment before.
    ///
    /// This is what `OskMode::Auto` actually decides on: raising the
    /// keyboard for every focused text-input regardless of hardware, which
    /// is what this compositor did before this field existed, is right for a
    /// tablet and wrong for the desk that has a keyboard and a mouse and
    /// nothing else — and there is no way to tell the two apart from the
    /// config file alone, since both would leave `osk` unset.
    pub osk_touch_seen: bool,
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
    /// The libei sockets handed out by the remote-desktop portal's
    /// ConnectToEIS, one per session that asked for one. See [`crate::libei`]
    /// — in particular for why a connection is remembered at all, which is
    /// that closing the session has to be able to close the socket.
    pub eis: crate::libei::Connections,
    /// The keymap the seat is using, as the config file described it.
    ///
    /// Kept because a keymap cannot be read back out of a seat in a form
    /// anything else can be given: `add_keyboard` takes an `XkbConfig` and
    /// hands back a handle, and a libei client has to be sent the *same*
    /// description or it composes keycodes against a layout this desk does not
    /// type in. Empty until a config file names one, which is the built-in
    /// keymap — the same default `Seat::add_keyboard` was given at startup.
    pub keyboard_config: crate::config::KeyboardConfig,
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
    /// Shares begun and not yet answered, each with the deadline that refuses
    /// it if PipeWire never names its node. The stream is already in `casts`;
    /// what waits here is the reply, and the right to tear down what would
    /// otherwise be a stream nobody can reach.
    pending_shares: Vec<PendingShare>,
    /// Which deadline the next share gets. Rising for the same reason as
    /// `next_pick`: an answer must never land on the share that replaced it.
    next_share: u64,
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
    pub pending_screenshots: Vec<crate::screenshot::PendingScreenshot>,
    /// Screenshot files written for the portal and not yet taken back.
    ///
    /// The path is paired with when it was written, because the client on the
    /// other end of the portal reply is the only thing that knows when it has
    /// finished reading, and it does not say: a grace period is what stands in
    /// for that. Reaped on the housekeeping tick; see `reap_screenshot_temps`.
    pub screenshot_temps: Vec<(std::path::PathBuf, std::time::Instant)>,
    /// Buffers a client has destroyed whose images a renderer may still be
    /// holding.
    ///
    /// The Vulkan renderer keeps one image per `wl_buffer` it has uploaded
    /// from shared memory, so a client that paints every frame is not
    /// reallocated and re-uploaded every frame. Those entries are keyed by the
    /// buffer's object id and there is nothing in a dead id to notice, so the
    /// compositor has to say — and until it did, every shm buffer any client
    /// ever destroyed left its image behind for the life of the session. An
    /// idle desktop hid it, because a client that is not painting is not
    /// making buffers; a screen share does not let anything be idle, which is
    /// how it showed up as VRAM climbing by a couple of hundred megabytes a
    /// minute for as long as a share was open.
    ///
    /// Queued rather than forgotten where it happens: a buffer dies on the
    /// client's dispatch, and the renderer it has to be forgotten from can be
    /// moved out of the state at that moment. Drained once a turn of the event
    /// loop, which is before any of it is moved anywhere.
    pub dead_buffers: Vec<smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer>,
    /// Offscreens kept between captures, by what they are.
    ///
    /// A capture composites into a buffer of its own and reads it back, and
    /// allocating one is a whole screen off the GPU: fifteen megabytes at
    /// 1440p, thirty times a second for as long as a shared-memory screen
    /// share is open. Allocated and freed at that rate it is churn a driver
    /// answers by holding on to memory, which reads as a share that costs a
    /// couple of hundred megabytes of VRAM a minute and gives none of it back.
    ///
    /// `Any` because what a renderer draws into is its own type — a DMA-BUF
    /// for Vulkan, a renderbuffer for GLES — and the capture paths are generic
    /// over it. Let go of when nothing is being captured; see
    /// `release_capture_scratch`.
    capture_scratch: Vec<Box<dyn std::any::Any>>,
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
    ///
    /// An external locker's surfaces, and only those. The built-in lock screen
    /// has none: it is drawn out of the shell's own buffer, which is one
    /// buffer across the whole layout and was already there.
    pub lock_surfaces:
        std::collections::HashMap<String, smithay::wayland::session_lock::LockSurface>,
    /// What locking means here: somebody else's locker, or the shell's own
    /// lock screen. Recomputed from `idle.lock_command` on every config load,
    /// so there is one answer and every caller of `lock_session` gets it.
    pub lock_mode: crate::lock::Mode,
    /// Which lock this is, counted up on every one.
    ///
    /// The shell says which lock it has drawn and which lock a password was
    /// typed at, and a message naming any other one is dropped. Without it a
    /// `session.lock.drawn` from the shell process that died at the last lock
    /// would stand in for the new one having drawn, and the compositor would
    /// put a page that is showing the desktop on screen over a locked session.
    pub lock_generation: u64,
    /// The lock the shell has said it has painted, and the shell frame count
    /// when it said so.
    ///
    /// Both halves are the fail-closed rule; see `lock_screen_is_drawing`.
    pub lock_shell_drawn: Option<(u64, u64)>,
    /// The attempt PAM is chewing on, if there is one.
    ///
    /// One at a time. A second Enter while the first is still in the stack is
    /// dropped rather than queued: a queue is a way to spend the compositor's
    /// memory on a locked session, and PAM's failure delay means a queue of
    /// ten is twenty seconds of a lock screen that answers nothing.
    pub lock_attempt: Option<u64>,
    /// Checks a password, on a thread of its own. See `crate::lock`.
    pub authenticator: crate::lock::Authenticator,
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
    /// The path to the active configuration file, if any.
    pub config_file_path: Option<std::path::PathBuf>,
    /// Which monitors have been configured by a message this session.
    ///
    /// What `config.save` writes down, and nothing else. Saving every head
    /// would freeze whatever mode the backend happened to pick for a screen
    /// nobody has an opinion about, and would leave a hand-written `outputs`
    /// block in the config file permanently shadowed by an overlay that only
    /// restates it. A monitor is in here because somebody asked for something
    /// about it.
    pub output_settings_touched: std::collections::HashSet<String>,
    /// Whether the configuration being applied is the config file's own,
    /// replayed.
    ///
    /// [`Self::apply_output_config`] goes through `output.configure` — which
    /// is the right thing, because the file and the socket should reach the
    /// hardware the same way — and that makes the two indistinguishable from
    /// inside the handler. They differ in exactly two places and this is what
    /// tells them apart: a replay must not count as somebody having an opinion
    /// (or the first reload would copy the whole file into the overlay), and a
    /// replay must not arm a revert (nobody is sitting in front of a
    /// confirmation dialog during startup, so the countdown would simply undo
    /// the file twelve seconds in).
    pub output_config_replay: bool,
    /// The monitors as they were before the change now on screen, held until
    /// somebody says they can see it.
    ///
    /// `None` is "nothing to undo". See
    /// [`ViewportState::arm_output_revert`] for why a monitor change is
    /// provisional at all.
    pub output_revert: Option<Vec<crate::output_management::HeadChange>>,
    /// The timer that undoes it. A timerfd for the reason every other deadline
    /// here is one: this one has to fire on a desktop where nothing is
    /// happening, and on a desktop whose screen has just gone black nothing is
    /// happening by definition.
    pub output_revert_timer: Option<std::os::fd::OwnedFd>,
    /// Which arming the pending tick belongs to, so the fallback loop timer —
    /// which cannot be re-armed, only added to — does not fire a deadline that
    /// was superseded by a second configuration.
    pub output_revert_generation: u64,
    /// The timer the config's live reload settles on, when watched.
    pub config_reload_timer: Option<std::os::fd::OwnedFd>,
    /// Whether a config reload is already waiting on the timer.
    pub config_reload_pending: bool,
    /// The timer the shell's live reload settles on, when `--watch-shell` set
    /// one up. Same reasoning again — see [`crate::shell_watch`].
    pub shell_reload_timer: Option<std::os::fd::OwnedFd>,
    /// Whether a reload is already waiting on the fallback loop timer, so a
    /// save touching twenty files queues one rather than twenty.
    pub shell_reload_pending: bool,
    /// How many ticks in a row have released nothing.
    pub barrier_quiet: u32,
    /// Whether any surface has ever been seen holding fifo or commit-timing
    /// state, which is what makes [`Self::barriers_outstanding`] a constant
    /// answer from then on.
    ///
    /// The fifo and commit-timing requests are answered inside Smithay; this
    /// compositor runs no code when one arrives, so the only place the state
    /// can be noticed is the walk `barriers_outstanding` exists to avoid. Once
    /// it has ever been seen, the walk is skipped and the question is answered
    /// "yes" for the rest of the session — a monotonic flag rather than a
    /// count, because there is no moment where disarming is visible either.
    pub barrier_ever_armed: std::cell::Cell<bool>,
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
            crate::render::ScreenElement<R>,
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

        // Have the compositor stand in for a real `zwp_input_method_v2`
        // client on this seat, permanently, for one reason only: without
        // *something* willing to be the seat's input method, this smithay
        // fork discards every `zwp_text_input_v3` request before
        // `active_text_input_id` is ever set, and with it discarded there is
        // no way to notice a client asking for text input at all — not to
        // type into it, which this compositor does through `inject_keysym`
        // instead (see the long comment on `Request::OskKey`), but even to
        // know a text field was focused so the on-screen keyboard can come up
        // on its own. Turning this on is what makes `sync_osk_wanted` in
        // `input.rs` see anything.
        //
        // Safe to leave on whether or not a real input method is also
        // running: `enter`/`leave` on a text-input already fire whenever
        // either `input_method.has_instance()` or this flag is true, so a
        // real IME binding later changes nothing a client can observe, and
        // nothing here ever calls `commit_string` on its own — the one method
        // this flag additionally unlocks — so there is no second writer for
        // a real input method to race.
        smithay::wayland::text_input::TextInputSeat::text_input(&seat)
            .set_compositor_input_method(true);

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
                // Dark unless a config file, an overlay or the chord says
                // otherwise, which is what `ViewportState::dark_mode` starts
                // on a few lines further down. The two are one setting and
                // this is the copy the shell is told about.
                dark_mode: true,
                // Filled in on the way out, from the keymap as it stands by
                // then: this struct is built before a config file has been
                // read and there is nothing yet to describe.
                binds: Vec::new(),
                bar: None,
                rules: None,
                theme: None,
                gaps: None,
                border: None,
                // Nothing said about the clock, which is the shell deciding
                // for itself: the locale the engine runs under and the hour
                // that locale writes.
                clock: None,
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
                // The same default OskMode::Auto is, spelled the way the
                // shell reads it back — see apply_config for where a config
                // file overrides both this and osk_mode together.
                osk: "auto".to_owned(),
            },
            shell_url: None,
            output_config: std::collections::HashMap::new(),
            output_memory: std::collections::HashMap::new(),
            startup: None,
            notifications: crate::notification::Notifications::default(),
            notification_history: crate::notification::History::default(),
            tray: crate::tray::Tray::default(),
            mpris: crate::mpris::Mpris::default(),
            power: crate::power::Power::default(),
            network: crate::network::Network::default(),
            bluetooth: crate::bluetooth::Bluetooth::default(),
            lid: crate::power::LidAction::default(),
            last_lid_closed: None,
            clipboard: crate::clipboard::Clipboard::default(),
            launcher_list: Vec::new(),
            launcher_generation: 0,
            launcher_scan: crate::launcher::Scanner::default(),
            // The tray's own default, which the first config load may replace.
            icon_theme: "hicolor".to_owned(),
            // On unless a file says otherwise: a desktop with no tray is a
            // desktop where several ordinary applications have nowhere to put
            // themselves, and nothing about that is discoverable.
            tray_enabled: true,
            appearance: crate::appearance::Appearance::default(),
            status: crate::status::Status::default(),
            idle: crate::idle::Idle::default(),
            idle_settings: crate::idle::Settings::default(),
            shortcuts: Vec::new(),
            shortcuts_held: Vec::new(),
            shortcuts_to_announce: Vec::new(),
            pending_shortcuts: None,
            shortcuts_store: crate::shortcuts::Store::load(crate::shortcuts::Store::default_path()),
            shortcut_signals: crate::shortcuts::Signals::default(),
            screensaver: None,
            bus_inhibitors: crate::inhibit::Registry::default(),
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
                std::env::var("VIEWPORT_MENU").ok().as_deref(),
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
            pending_shells: Vec::new(),
            next_shell_id: 0,
            shell_url_spans: false,
            dispatch_origin: (0, 0).into(),
            dispatch_client: 0,
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
            magnifier: crate::magnify::Magnifier::default(),
            cursor_hide_armed: false,
            cursor_hide_timer: None,
            last_layout: None,
            needs_render: false,
            dirty_outputs: std::collections::HashSet::new(),
            needs_restack: false,
            needs_colour_notify: false,
            needs_foreign_outputs: false,

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
            osk_wanted: false,
            osk_mode: crate::config::OskMode::Auto,
            xwayland_scale: crate::config::XwaylandScale::Off,
            osk_touch_seen: false,
            gamma_state,
            output_power_state,
            pipewire: None,
            casts: Vec::new(),
            eis: crate::libei::Connections::default(),
            keyboard_config: crate::config::KeyboardConfig::default(),
            pointer_drag: None,
            pointer_on_shell: false,
            pointer_grabbed_by_shell: false,
            picker: None,
            next_pick: 1,
            pending_shares: Vec::new(),
            next_share: 1,
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
            pending_screenshots: Vec::new(),
            screenshot_temps: Vec::new(),
            dead_buffers: Vec::new(),
            capture_scratch: Vec::new(),
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
            lock_mode: crate::lock::Mode::BuiltIn,
            lock_generation: 0,
            lock_shell_drawn: None,
            lock_attempt: None,
            authenticator: crate::lock::Authenticator::default(),
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
            config_file_path: None,
            output_settings_touched: std::collections::HashSet::new(),
            output_config_replay: false,
            output_revert: None,
            output_revert_timer: None,
            output_revert_generation: 0,
            config_reload_timer: None,
            config_reload_pending: false,
            shell_reload_timer: None,
            shell_reload_pending: false,
            barrier_quiet: 0,
            barrier_ever_armed: std::cell::Cell::new(false),
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
}

include!("state/capture.rs");

impl ViewportState {
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
        // A re-placement is a move between screens as far as a taskbar is
        // concerned — an output that went dark took its windows with it.
        self.sync_foreign_outputs();
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
        // And anything remote that was told where the monitors are. A libei
        // client points in the layout's own coordinates, which it was handed a
        // description of when its devices were made — see
        // `crate::libei::ViewportState::refresh_eis_regions`. This is the one
        // call every rearrangement goes through, and it costs nothing at all
        // when nobody is connected, which is nearly always.
        self.refresh_eis_regions();
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
            Ok(()) => {
                tracing::info!(
                    "{}: {}x{}@{}",
                    output.name(),
                    mode.size.w,
                    mode.size.h,
                    mode.refresh
                );
                // The mode is half of what a tearing refusal was measured
                // under, so the answer may have changed with it.
                surface.clear_tearing_refusal();
            }
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
            // Nothing will draw this screen while it is off, and screencopy
            // requests are served from the draw — so the ones waiting on it
            // are told now rather than left holding their buffers until the
            // housekeeping tick finds them. The tick still covers this; this
            // is just not making the client wait a second for the news.
            self.drop_pending_copies_for(output);
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
            Ok(()) => {
                tracing::info!(
                    "adaptive sync {} on {}",
                    if enabled { "on" } else { "off" },
                    output.name()
                );
                // The conditions a tearing refusal was measured under just
                // changed, so the answer may have changed with it.
                surface.clear_tearing_refusal();
            }
            // Most panels cannot, and asking is how you find out. Nothing
            // changed, so nothing is cleared.
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

    /// Drop the images the renderers cached for buffers their clients have
    /// destroyed.
    ///
    /// See `dead_buffers` for why this is a queue and why nothing else would
    /// ever clear it. Only the Vulkan renderer keeps a cache keyed by the
    /// buffer: the GLES one hangs its shm textures off the surface, which
    /// Smithay drops when the surface goes.
    pub fn forget_dead_buffers(&mut self) {
        if self.dead_buffers.is_empty() {
            return;
        }
        let Some(mut udev) = self.udev.take() else {
            // Nested and headless draw with GLES, which caches nothing by
            // buffer — so there is nothing to forget and no reason to keep the
            // queue.
            self.dead_buffers.clear();
            return;
        };
        // A renderer that is out on loan cannot be told anything. The queue
        // keeps until the next turn of the loop, by which time it is back.
        if udev
            .devices
            .iter()
            .any(|device| matches!(device.renderer, crate::udev::Gpu::Placeholder))
        {
            self.udev = Some(udev);
            return;
        }
        for device in &mut udev.devices {
            if let crate::udev::Gpu::Vulkan(renderer) = &mut device.renderer {
                for buffer in &self.dead_buffers {
                    renderer.forget_shm_buffer(buffer);
                }
            }
        }
        tracing::trace!("forgot {} destroyed buffer(s)", self.dead_buffers.len());
        self.dead_buffers.clear();
        self.udev = Some(udev);
    }

    /// A buffer to composite a capture into, reused if one of the same shape
    /// is already in hand.
    ///
    /// Taken out rather than borrowed, so the caller can bind it — and handed
    /// back with `keep_capture_target` once the pixels are out of it. A path
    /// that fails in between simply drops it, which is what every capture used
    /// to do with every buffer it made.
    fn take_capture_target<R, B>(
        &mut self,
        renderer: &mut R,
        format: smithay::backend::allocator::Fourcc,
        size: smithay::utils::Size<i32, smithay::utils::Buffer>,
    ) -> Result<B, String>
    where
        R: Offscreen<B>,
        B: 'static,
        B: 'static,
    {
        if let Some(buffer) = take_scratch(&mut self.capture_scratch, format, size) {
            return Ok(buffer);
        }
        renderer
            .create_buffer(format, size)
            .map_err(|e| format!("allocating a capture target: {e}"))
    }

    /// Keep a capture's buffer for the next one of the same shape.
    fn keep_capture_target<B>(
        &mut self,
        format: smithay::backend::allocator::Fourcc,
        size: smithay::utils::Size<i32, smithay::utils::Buffer>,
        buffer: B,
    ) where
        B: 'static,
    {
        keep_scratch(&mut self.capture_scratch, format, size, buffer);
    }

    /// Let the capture buffers go once nothing is being captured.
    ///
    /// Called once a turn of the event loop, which is where a share that has
    /// just stopped is noticed. Holding them for a desktop nobody is recording
    /// is several screens of VRAM for nothing.
    pub fn release_capture_scratch(&mut self) {
        if self.capture_scratch.is_empty() {
            return;
        }
        if self.casts.is_empty()
            && self.pending_copies.is_empty()
            && self.pending_capture_frames.is_empty()
        {
            self.capture_scratch.clear();
        }
    }
}

include!("state/screencast.rs");

impl ViewportState {
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
                Ok(()) => {
                    tracing::info!(
                        "adaptive sync {} on {}",
                        if enabled { "on" } else { "off" },
                        surface.output.name()
                    );
                    // The conditions a tearing refusal was measured under
                    // just changed, so the answer may have changed with it.
                    surface.clear_tearing_refusal();
                }
                // Not an error worth stopping for: most panels do not do it,
                // and asking is how you find out. Nothing changed, so nothing
                // is cleared.
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
                surface.queued_at = None;
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
            // come back. The clock on it goes too: the watchdog measures a
            // stall from `queued_at`, and a flip abandoned here is not a GPU
            // that stopped answering.
            surface.pending = false;
            surface.queued_at = None;
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
        // Either list is enough. A film inhibits over the bus and a full-screen
        // game over Wayland, and a session that honoured only the protocol
        // would blank under the first of those — which is most of them.
        let inhibited = !self.idle_inhibitors.is_empty() || self.bus_inhibitors.inhibited();
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

    /// A snapshot from the UPower worker: paint the bar, and act on the lid
    /// if it moved.
    pub fn handle_power(&mut self, snapshot: viewport_ipc::event::PowerSnapshot) {
        let closed = snapshot.lid_closed;
        if self.last_lid_closed != Some(closed) {
            self.last_lid_closed = Some(closed);
            if self.lid != crate::power::LidAction::Ignore {
                if closed {
                    self.apply_lid_close();
                } else {
                    self.set_outputs_enabled(true);
                }
            }
        }
        if self.power.widget() {
            self.notify(&viewport_ipc::Event::PowerUpdate(snapshot));
        }
    }

    /// The lid just closed.
    pub fn apply_lid_close(&mut self) {
        match self.lid {
            crate::power::LidAction::Ignore => {}
            crate::power::LidAction::Lock => self.lock_session(),
            crate::power::LidAction::Blank => self.blank_screens(),
            crate::power::LidAction::Suspend => self.power.suspend(),
        }
    }

    /// Lock the session, however this machine is configured to.
    ///
    /// The one answer to what locking means, and every way of asking for it
    /// comes through here: the idle deadline, the `lock` binding, the lid
    /// action, and the power menu's Lock row over `session.lock`. That was
    /// already the property this function was written for
    /// (`src/binding.c:614`); it matters more now that there are two things it
    /// can mean. Which one is `self.lock_mode`, worked out once per config
    /// load — see `crate::lock::Mode`.
    pub fn lock_session(&mut self) {
        // Not over a locker that is already up.
        //
        // The lock handler refuses the second lock, but by then a whole second
        // locker has been started, has authenticated nobody, and is waiting to
        // be told `finished`. Cheaper and quieter to not run it: the idle
        // deadline fires on a session somebody locked by hand five minutes
        // earlier, which is how two swaylocks ended up on one screen.
        //
        // A locked session with no locker drawing is *not* this, and is left
        // alone deliberately — running another locker against it is the way
        // out of a locker that crashed, and `check_lock_screen` says so.
        if self.locked && self.lock_screen_is_drawing() {
            tracing::info!("lock: a locker is already drawing; leaving it alone");
            return;
        }

        match self.lock_mode.clone() {
            crate::lock::Mode::Command(command) => {
                let display = self.child_display_env();
                crate::input::spawn_with_env(&command, &display)
            }
            // No locker configured, which used to be a warning and nothing
            // else: `lock` bound to a chord did nothing, and the lid and the
            // idle deadline did nothing either. Now it is the shell's own lock
            // screen. See `crate::lock` for why that is the better default and
            // what a desk with no keyboard could not do before it.
            crate::lock::Mode::BuiltIn => self.lock_with_shell(),
        }
    }

    /// Lock the session and ask the shell to draw the lock screen.
    ///
    /// The compositor takes the lock itself here rather than waiting for a
    /// client to ask for one, which is the whole difference between this and
    /// the ext-session-lock path: there is no second process to crash, so
    /// there is nothing to wait for. Everything the protocol handler does on a
    /// `lock` request is done here for the same reasons, and the comments
    /// there are the argument for each of them.
    ///
    /// What the page then has to do is in `Event::SessionLock`. What happens
    /// if it does not is the point of `lock_screen_is_drawing`: the session is
    /// locked from this line onwards whatever the shell does next, and nothing
    /// of the shell's buffer reaches the screen until it has said it has drawn
    /// *and* painted a frame after saying so.
    fn lock_with_shell(&mut self) {
        // A new lock is a new generation, always — including a re-lock of a
        // session that is already locked with a shell that has stopped
        // drawing. The old generation's `drawn` must not carry over, because
        // the page that sent it is the page that stopped.
        self.lock_generation = self.lock_generation.wrapping_add(1);
        self.lock_shell_drawn = None;
        self.lock_attempt = None;
        self.locked = true;
        self.locked_at = Some(std::time::Instant::now());
        self.lock_warned = false;
        self.lock_surfaces.clear();

        let generation = self.lock_generation;
        let can_authenticate = self.authenticator.online();
        if !can_authenticate {
            // Loud, because from the front this is a password box that will
            // never open and there is nothing on screen that could explain
            // why. Locked anyway: a session that refuses to lock because it
            // cannot check a password is a laptop that goes into a bag with
            // the desktop on screen.
            tracing::error!(
                "lock: no authentication worker, so no password can be checked. \
                 The session is locked all the same — the way out is another VT \
                 (Ctrl+Alt+F1..F12) or an idle.lock_command that runs a locker \
                 of its own."
            );
        }
        tracing::info!("session locked; the shell draws the lock screen (lock {generation})");
        self.notify(&viewport_ipc::Event::SessionLock {
            generation,
            can_authenticate,
        });

        // The keyboard goes to the shell, which is the opposite of what the
        // protocol handler does and is right for the same reason. There, focus
        // is dropped because the locker has not made its surface yet and the
        // window that had the keyboard must not keep it; here the surface that
        // will draw the lock screen already exists, and the password has to go
        // somewhere. Nothing else can reach it: `surface_under` answers with
        // the shell and nothing else while the session is locked, and no
        // binding fires.
        self.focus_lock_shell();
        self.needs_render = true;
    }

    /// Put the keyboard on the shell for a lock it is drawing.
    ///
    /// Called at the lock and again whenever the shell restarts under one, so
    /// a page that crashed and came back is typable without the person having
    /// to find the mouse — which on the desk this feature exists for is not a
    /// thing they have.
    pub fn focus_lock_shell(&mut self) {
        if !self.locked || !self.lock_mode.is_built_in() {
            return;
        }
        if !self.focus_shell_at(None) {
            // No shell client to focus. Either the WPE backend, which is not a
            // client and takes keys another way, or a compositor running with
            // no shell at all — a test. Focus goes nowhere rather than staying
            // on the window that had it.
            if let Some(keyboard) = self.seat.get_keyboard() {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, None, serial);
            }
        }
    }

    /// The shell says it has painted the lock screen.
    ///
    /// Recorded with the frame count at the moment it was said, which is the
    /// half of the rule that a message cannot fake: see
    /// `lock_screen_is_drawing`.
    pub fn lock_screen_drawn(&mut self, generation: u64) {
        if !self.locked || !self.lock_mode.is_built_in() {
            return;
        }
        if generation != self.lock_generation {
            tracing::debug!(
                "lock: the shell says it drew lock {generation}, but this is lock {}",
                self.lock_generation
            );
            return;
        }
        if self.lock_shell_drawn.map(|(lock, _)| lock) == Some(generation) {
            return;
        }
        tracing::info!("lock: the shell has drawn lock {generation}");
        self.lock_shell_drawn = Some((generation, self.shell_frames));
        self.needs_render = true;
    }

    /// Somebody typed a password at the lock screen.
    ///
    /// Handed to the worker thread and answered later; nothing here waits.
    /// Every refusal below answers the page rather than dropping the message,
    /// because a lock screen whose Enter key does nothing is indistinguishable
    /// from one that is broken, and the person's next move is to hold the
    /// power button.
    pub fn try_unlock(&mut self, generation: u64, password: viewport_ipc::request::Secret) {
        if !self.locked || !self.lock_mode.is_built_in() {
            // Nothing to unlock, or a locker of somebody else's is holding it
            // — in which case this compositor has no business checking a
            // password on its behalf, and unlocking on one would be a way
            // past a lock screen it does not own.
            return;
        }
        if generation != self.lock_generation {
            tracing::debug!("lock: a password arrived for lock {generation}, which is over");
            return;
        }
        if self.lock_attempt.is_some() {
            tracing::debug!("lock: an attempt is already with PAM; dropping this one");
            return;
        }
        if !self.authenticator.ask(crate::lock::Attempt {
            generation,
            password,
        }) {
            self.notify(&viewport_ipc::Event::SessionLockError {
                generation,
                message: "this session cannot check a password".to_owned(),
            });
            return;
        }
        self.lock_attempt = Some(generation);
    }

    /// What PAM said about it.
    pub fn handle_lock_verdict(&mut self, verdict: crate::lock::Verdict) {
        if self.lock_attempt == Some(verdict.generation) {
            self.lock_attempt = None;
        }
        if !self.locked || verdict.generation != self.lock_generation {
            // The lock ended while the stack was thinking — a takeover, a
            // `viewport msg`. The verdict is about a lock that is over, and a
            // true one must not unlock the lock that came after it.
            return;
        }
        if verdict.ok {
            tracing::info!("lock: the password was accepted");
            self.unlock_session();
            return;
        }
        let message = verdict
            .message
            .unwrap_or_else(|| "that password was not accepted".to_owned());
        // At info, not warn, and without a user name: a wrong password at a
        // lock screen is the ordinary case — somebody typing with the caps
        // lock on — and a log that shouts about it teaches people to ignore
        // the log.
        tracing::info!("lock: refused — {message}");
        self.notify(&viewport_ipc::Event::SessionLockError {
            generation: verdict.generation,
            message,
        });
    }

    /// Take the built-in lock screen down.
    ///
    /// Only ever called with a verdict behind it. There is no other caller and
    /// deliberately no IPC message that reaches it: an `unlock` on the control
    /// socket would be a lock screen anything on the machine could dismiss.
    fn unlock_session(&mut self) {
        self.locked = false;
        self.locked_at = None;
        self.lock_warned = false;
        self.lock_shell_drawn = None;
        self.lock_attempt = None;
        self.lock_surfaces.clear();
        tracing::info!("session unlocked");
        self.notify(&viewport_ipc::Event::SessionUnlock);
        // Back to whatever the desktop decides, which for an empty desk is the
        // shell and for a desk with windows is the window that had the
        // keyboard before the lock. `focus_shell_if_idle` is the floor under
        // both; a window that wants the keyboard back takes it on the next
        // click, exactly as it would after any other loss of focus.
        self.focus_shell_if_idle();
        self.needs_render = true;
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

    /// Snapshot the monitors and start the clock on putting them back.
    ///
    /// A monitor change is the one setting that can take away the thing you
    /// would need in order to undo it. A mode the panel will drive but the
    /// display will not, a scale that puts the dialog off the edge, a rotation
    /// on the wrong screen, the wrong monitor switched off — every one of
    /// those ends with a person looking at a black rectangle and no way back
    /// except a TTY. Every desktop that lets a screen be reconfigured answers
    /// this the same way, and so does `docs/ipc.md`, which has described the
    /// countdown since before there was one: the change applies, and it comes
    /// back off in twelve seconds unless somebody says they can see it.
    ///
    /// That promise was documented and not implemented — `output.confirm` was
    /// a handler with an empty body and a comment saying nothing arms a
    /// revert. Anything that read the documentation and skipped the
    /// confirmation therefore kept a mode that had blanked the screen, which
    /// is the exact failure the sentence was written to rule out.
    ///
    /// The snapshot is taken *before* the change, and only when there is not
    /// one already: two configurations inside the window are one change as far
    /// as undoing goes, and the state worth going back to is the one from
    /// before the first of them. A panel setting a mode and a scale as two
    /// messages is the ordinary case of that, not a corner.
    pub fn arm_output_revert(&mut self) {
        if self.output_revert.is_none() {
            let before: Vec<crate::output_management::HeadChange> = self
                .heads()
                .into_iter()
                .map(|head| {
                    let mode = head.output.current_mode();
                    crate::output_management::HeadChange {
                        name: head.output.name(),
                        enabled: head.enabled,
                        mode,
                        // Whether going back would mean programming a modeline
                        // the connector never offered. It cannot, for a mode
                        // the connector is driving right now — but the field is
                        // what `apply_output_configuration` uses to tell a mode
                        // that may not work from one that is known to, and
                        // answering it from the list rather than assuming keeps
                        // the restore honest on a nested backend, where custom
                        // modes are real.
                        custom_mode: mode.is_some_and(|mode| !head.output.modes().contains(&mode)),
                        position: Some(head.position),
                        transform: Some(head.output.current_transform()),
                        scale: Some(head.output.current_scale().fractional_scale()),
                        adaptive_sync: Some(head.adaptive_sync),
                    }
                })
                .collect();
            self.output_revert = Some(before);
        }

        self.output_revert_generation = self.output_revert_generation.wrapping_add(1);
        let generation = self.output_revert_generation;

        if self.output_revert_timer.is_none() {
            self.output_revert_timer = self.create_tick("output revert", Self::output_revert_tick);
        }
        if Self::arm_tick(
            "output revert",
            self.output_revert_timer.as_ref(),
            OUTPUT_REVERT_AFTER,
        ) {
            return;
        }

        // No timerfd. A loop timer still fires wherever calloop is the one
        // waiting, which is every backend but the web engine's — and a
        // countdown that does not run is only ever a countdown that does not
        // save you, so it is worth having the weaker version of.
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(OUTPUT_REVERT_AFTER);
        if let Err(e) = self
            .loop_handle
            .insert_source(timer, move |_, _, state: &mut Self| {
                if state.output_revert_generation == generation {
                    state.output_revert_tick();
                }
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            })
        {
            tracing::warn!("output revert: {e}");
        }
    }

    /// Nobody said they could see it. Put the monitors back.
    pub(crate) fn output_revert_tick(&mut self) {
        let Some(before) = self.output_revert.take() else {
            return;
        };
        tracing::warn!(
            "no output.confirm within {OUTPUT_REVERT_AFTER:?}; \
             putting the monitors back where they were"
        );
        // Through the same path wlr-output-management applies a configuration
        // rather than a restore written twice: it revalidates, it refuses to
        // leave the desk with every screen off, and it runs the tail — the
        // windows remapped, the clients told, the frame asked for — that a
        // half-written undo would forget.
        if !self.apply_output_configuration(&before, false) {
            tracing::error!("could not put the monitors back; leaving them as they are");
        }
        self.advertise_outputs();
        self.notify_output_layout();
        self.needs_render = true;
    }

    /// Somebody can see it: drop the snapshot and let the deadline lapse.
    ///
    /// The timer is not disarmed, only orphaned. Its tick finds no snapshot
    /// and does nothing, which is one branch rather than two syscalls and two
    /// ways for the two to disagree.
    pub fn confirm_output_revert(&mut self) {
        if self.output_revert.take().is_some() {
            tracing::info!("the output configuration was confirmed");
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
        // See `output_config_replay`: what follows is the file being put back
        // into effect, not somebody at a panel changing their mind.
        let was_replay = std::mem::replace(&mut self.output_config_replay, true);
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
                enabled: want.enabled,
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
                transform: want.transform.as_deref().and_then(|text| {
                    let parsed = parse_transform(text);
                    if parsed.is_none() {
                        // Said like the neighbours say it: a name this does not
                        // know is warned about with the key it came from, and
                        // `None` leaves the output's transform as it was.
                        tracing::warn!(
                            "outputs.{name}.transform {text:?} is not one of normal, 90, \
                             180, 270, flipped, flipped-90, flipped-180 or flipped-270"
                        );
                    }
                    parsed
                }),
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
        self.output_config_replay = was_replay;
        self.output_config = outputs;
    }

    /// What an output should show, worked out without a renderer.
    ///
    /// Everything the backend would otherwise have to reach into this state
    /// for while its renderer is borrowed. The two backends share it, which is
    /// what stops the nested one drifting into showing something different
    /// from the real thing.
    /// The region one output is showing while the magnifier is on.
    ///
    /// `None` for every output but the one the pointer is on, and for all of
    /// them when the magnifier is off. Only one output is magnified because
    /// the region follows the pointer and the pointer is on one screen at a
    /// time — see the header of [`crate::magnify`] for why the alternatives
    /// are each wrong in their own way.
    ///
    /// One definition, consulted by both halves: the renderer asks it what to
    /// draw and the touch path asks it what a spot on the glass is over, and
    /// if those two ever answered differently a finger would land somewhere
    /// other than where it was put.
    pub fn magnified_view(&self, output: Rectangle<i32, Logical>) -> Option<crate::magnify::View> {
        if !self.magnifier.is_on() {
            return None;
        }
        let at = self.seat.get_pointer()?.current_location();
        if !output.to_f64().contains(at) {
            return None;
        }
        Some(crate::magnify::View::new(output, self.magnifier.zoom(), at))
    }

    /// What is under a place on the glass.
    ///
    /// For input that names a *position on the panel* rather than a movement:
    /// a touchscreen, and a tablet in absolute mode. Under magnification the
    /// panel is showing a blown-up piece of the layout, so the layout point a
    /// finger is on is not the one the fraction it reported scales to.
    ///
    /// Nothing else calls this, and nothing else should. A mouse reports how
    /// far it moved, and how far the cursor moves is not affected by what the
    /// screen is doing with the picture — putting a mouse through this would
    /// divide its every movement by the zoom, which is a pointer that crawls.
    /// See the header of [`crate::magnify`].
    pub fn glass_to_content(&self, at: Point<f64, Logical>) -> Point<f64, Logical> {
        if !self.magnifier.is_on() {
            return at;
        }
        let Some(output) = self.space.outputs().find_map(|output| {
            let geometry = self.space.output_geometry(output)?;
            geometry.to_f64().contains(at).then_some(geometry)
        }) else {
            return at;
        };
        match self.magnified_view(output) {
            Some(view) => view.to_content(at),
            // The touch landed on a screen that is not the magnified one, so
            // what is on it is what it looks like.
            None => at,
        }
    }

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
                //
                // The ids are the view's own, and there is nowhere else to get
                // them: a border side is one element frame after frame, and an
                // element whose id changes is an element the damage tracker
                // believes is new. A window with no view has no border to
                // draw, so this is also the only branch that needs any — four
                // fresh ids were minted per window per output per frame for
                // the case that never reaches them.
                let overlay: Vec<_> = view
                    .filter(|_| drawn_on_this_output)
                    .and_then(|view| {
                        view.frame
                            .map(|frame| (frame, view.box_, view.scale, &view.overlay_ids))
                    })
                    .map(|(frame, hole, drawn_at, overlay_ids)| {
                        crate::render::border_sides(frame, hole, drawn_at)
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

                // The corners of that border, which the sides above do not
                // reach: the curve crosses into the hole, and inside the hole
                // the shell is behind whatever this window is floating over.
                // Only for a rounded window — a square one's border is the
                // four sides and nothing else.
                //
                // The wedges the border's curve occupies inside the hole, and
                // not the corner squares that hold them: the rest of each
                // square is the hole itself, which in the shell's buffer is
                // the desktop's own background. Drawing that over the window a
                // floating one is lifted above puts four triangles of
                // wallpaper through it — and a client that does not fill its
                // hole to the pixel, which is every terminal, leaves room for
                // exactly that.
                let corners = view
                    .filter(|_| drawn_on_this_output && radius > width)
                    .and_then(|view| {
                        view.frame
                            .map(|frame| (frame, view.box_, view.scale, &view.corner_id))
                    })
                    .and_then(|(frame, hole, drawn_at, corner_id)| {
                        let hole = crate::render::drawn_hole_of(hole, drawn_at);
                        let hole = crate::render::overlay_side(hole, output_geometry)?;
                        let hole = hole.to_f64().to_physical(scale).to_i32_round();
                        let wedges = crate::rounded::cutaway(hole, physical(radius - width));
                        // Held inside the frame's own outer arc. The wedge is
                        // a copy of the shell's buffer, and with a radius much
                        // past the border's width the hole's square corner
                        // pokes *outside* the rounded frame — where the buffer
                        // is not border but whatever the page drew behind the
                        // frame, which over another window is the wallpaper.
                        // That was three or four pixels of it at each corner.
                        let frame = crate::render::overlay_side(frame, output_geometry)?;
                        let frame = frame.to_f64().to_physical(scale).to_i32_round();
                        let wedges = crate::rounded::clip_to(
                            wedges,
                            &crate::rounded::bands_within(frame, physical(radius)),
                        );
                        (!wedges.is_empty()).then(|| (corner_id.clone(), wedges))
                    });

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
                    corners,
                })
            })
            .collect();

        // How many popups are about to be drawn, said when it changes. A menu
        // that is created, configured and then drawn zero times is a
        // different fault from one that is drawn somewhere unhelpful.
        //
        // Only when something is listening. The census walks every popup of
        // every window and keys its tally by `output.name()`, which allocates
        // — all of it per output per frame, to decide whether to emit a line
        // that a session at the default level discards.
        if tracing::enabled!(tracing::Level::DEBUG) {
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

        // Whether the desktop page's rectangle covers this whole screen, for
        // the lock screen alone. Every other use of the shell's buffer is
        // happy to cover part of a monitor and leave the rest to the clear
        // colour; a lock screen is not, because the part it does not cover is
        // the part the desktop would show through.
        #[allow(unused_mut)]
        let mut shell_covers_output = self.shell_clients.iter().any(|page| {
            page.desktop && page.owned.is_some() && page.region.contains_rect(output_geometry)
        });
        #[cfg(feature = "wpe")]
        {
            shell_covers_output |= self.shells.iter().any(|page| {
                page.desktop && page.owned.is_some() && page.region.contains_rect(output_geometry)
            });
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
            // The magnified region, on the output the pointer is on and no
            // other. `output_geometry` rather than the output's own mode: the
            // magnifier works in the layout's logical coordinates, which is
            // what the pointer is in and what the hit test is in, and being
            // in the same space as those is the whole reason nothing else has
            // to be told about it.
            magnify: self.magnified_view(output_geometry),
            scale,
            // Nothing is locked on nearly every frame this compositor ever
            // draws, and `output.name()` allocates a `String` to ask — so the
            // empty map is answered without building the key for it.
            lock: (!self.lock_surfaces.is_empty())
                .then(|| self.lock_surfaces.get(&output.name()))
                .flatten()
                // A locker that exited leaves its surfaces behind until the
                // next housekeeping tick; drawing one is drawing nothing.
                .filter(|lock| smithay::utils::IsAlive::alive(lock.wl_surface()))
                .map(|lock| lock.wl_surface().clone()),
            locked_blank: self.locked,
            // Drawn only where all three hold: the session is locked with the
            // built-in screen, the shell has drawn one for *this* lock and
            // painted since saying so, and its rectangle covers this monitor.
            // Anything short of all three is a black screen, which is the side
            // to fail on.
            shell_lock: self.locked
                && self.lock_mode.is_built_in()
                && shell_covers_output
                && self.lock_screen_is_drawing(),
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
                        // Read, not obeyed: a panic in whichever client thread
                        // held this last says nothing about the hotspot, and
                        // this runs every frame — a poisoned lock here would be
                        // a permanent cursor, not a diagnosis.
                        .map(|attrs| attrs.lock().unwrap_or_else(|e| e.into_inner()).hotspot)
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
                    Some(image) => {
                        let at = local.to_i32_round() - image.hotspot;
                        crate::render::Cursor::Image(image, at)
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
    /// Lazily is tempting — a session with no X client never needs it — but an
    /// X program started from a menu should just work, which it does because
    /// everything spawned on this compositor's behalf is told `DISPLAY`
    /// outright; see [`Self::child_display_env`].
    pub fn start_xwayland(&mut self, loop_handle: &LoopHandle<'static, Self>) {
        use smithay::xwayland::{XWayland, XWaylandEvent};

        // What X11 clients are told about this desk's density, if anything.
        // Absent from the config file this is 1, which is what every X11
        // client here has always got: a buffer of logical pixels that the
        // compositor magnifies onto whatever the panel is. See
        // `docs/protocols.md` for why that is the default and what the cost
        // of the alternative is.
        //
        // Both sources of scale, because neither alone is complete when this
        // runs: the backend is up so the live outputs exist, but an output
        // that is switched off is not in the space, and on a first start the
        // config file's block has been applied to outputs that may not have
        // arrived. Taking the largest of the union is the whole policy —
        // `pick_xwayland_scale`, which is where the mixed-DPI case is argued.
        let scale = crate::config::pick_xwayland_scale(
            self.xwayland_scale,
            self.space
                .outputs()
                .map(|output| output.current_scale().fractional_scale())
                .chain(
                    self.output_config
                        .values()
                        .filter_map(|output| output.scale),
                ),
        );

        // Xwayland's own environment, which is not this process's and not a
        // spawned child's either — it is the one process for which an X11
        // cursor size is the right thing to say. `child_display_env` cannot
        // carry this: it goes to everything the compositor launches, and
        // whether a launched program will turn out to be an X11 client or a
        // Wayland one is not knowable from here. XCURSOR_SIZE handed to a
        // Wayland client that already scales its own cursor is a pointer at
        // twice the size on the same screen.
        //
        // Sized up with the scale for the same reason the windows are: with a
        // client scale of 2 the X server's pixels are half a logical pixel
        // each, so the 24-pixel cursor an X client loads is a 12-pixel cursor
        // on screen unless it is asked for at 48.
        let mut envs: Vec<(String, String)> = Vec::new();
        // The DPI Xwayland reports on its screen, which is what the toolkits
        // that never learned about window scaling read — Qt 6, Chromium, and
        // anything using Xft directly. 96 is X11's canonical density and the
        // number every toolkit falls back to, so the scaled desk is a whole
        // multiple of it.
        let mut extra_args: Vec<String> = Vec::new();
        if scale > 1 {
            envs.push((
                "XCURSOR_THEME".to_owned(),
                self.cursor_theme.name().to_owned(),
            ));
            envs.push((
                "XCURSOR_SIZE".to_owned(),
                (self.cursor_theme.size() * scale).to_string(),
            ));
            extra_args.push("-dpi".to_owned());
            extra_args.push((96 * scale).to_string());
        }

        let (xwayland, client) = match XWayland::spawn(
            &self.display_handle,
            None,
            envs,
            extra_args,
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

        // The other half of the scale, and the half that makes it correct
        // rather than merely large.
        //
        // A client scale is an extra mapping between this compositor's
        // logical coordinates and one client's: at 2 the X server is told its
        // outputs are twice as many pixels across, and everything it sends
        // back — buffers, surface positions, the window geometry the X11
        // window manager reads — is divided by two on the way in. So an X
        // client that draws its 800x600 window at 2x makes a 1600x1200 buffer
        // on a 1600x1200 X screen, and lands on the desk as 800x600 logical
        // pixels with four times the detail. Without this the same client
        // would simply be twice the size it asked to be, which is what
        // setting GDK_SCALE by hand on an unpatched compositor does and why
        // that advice comes with a patched Xwayland attached.
        //
        // Set before the event loop turns, which is what makes it safe: the
        // server is a process that has not connected yet, and no output has
        // been sent to it at the old scale.
        let scale = if scale > 1 {
            match client.get_data::<smithay::xwayland::XWaylandClientData>() {
                Some(data) => {
                    data.compositor_state.set_client_scale(scale as f64);
                    tracing::info!(
                        "X11 clients are scaled {scale}x (xwayland.scale is {})",
                        self.xwayland_scale.as_str()
                    );
                    scale
                }
                // Nothing to do but say so, and stand the whole thing down
                // together: telling every toolkit to draw at 2x while the
                // surfaces are still measured at 1x is the one combination
                // that is actively wrong, and it is wrong by a factor of two
                // in the direction of "nothing fits on the screen".
                None => {
                    tracing::error!(
                        "the Xwayland client carries no compositor state, so it cannot be \
                         scaled; X11 clients stay at 1x"
                    );
                    1
                }
            }
        } else {
            scale
        };

        let display_handle = self.display_handle.clone();
        let handle = loop_handle.clone();
        let inserted = loop_handle.insert_source(xwayland, move |event, _, state| match event {
            XWaylandEvent::Ready {
                x11_socket,
                display_number,
            } => {
                match X11Wm::start_wm(handle.clone(), &display_handle, x11_socket, client.clone()) {
                    Ok(mut wm) => {
                        // The half of the scale that the clients themselves
                        // have to act on. The compositor has made the X screen
                        // bigger in X pixels; nothing yet has told anything
                        // drawing on it to use them, and an application that
                        // is not told comes out crisp and half the size.
                        //
                        // XSETTINGS rather than the environment, and not only
                        // because writing environ behind the worker threads is
                        // the hazard `child_display_env` exists to avoid:
                        // GDK_SCALE reaches a program this compositor spawned
                        // and nothing started from a terminal, an ssh session
                        // or a systemd unit, while a setting on the X server
                        // reaches every client that ever connects to it. The
                        // three keys are the ones GNOME's settings daemon has
                        // published for a decade, which is why the toolkits
                        // read them:
                        //
                        //   Gdk/WindowScalingFactor  GTK's integer window
                        //       scale — the one that makes GTK draw more
                        //       pixels rather than larger ones.
                        //   Gdk/UnscaledDPI          the font DPI *before*
                        //       that factor, so GTK does not multiply the
                        //       text by the scale twice.
                        //   Xft/DPI                  the density everything
                        //       else reads: Qt 6, Chromium, Xft directly.
                        //
                        // Both DPI values are in 1024ths of a point, which is
                        // the unit the XSETTINGS registry defines for them.
                        //
                        // What this does not reach is written down in
                        // `docs/protocols.md`: Qt 5 without
                        // QT_AUTO_SCREEN_SCALE_FACTOR, Java, SDL games,
                        // xterm, and anything else that has no notion of a
                        // scale factor draws its 1x pixels into a screen
                        // whose pixels are now half the size, and comes out
                        // sharp and small.
                        if scale > 1 {
                            use smithay::xwayland::xwm::settings::Value;
                            let dpi = 96 * 1024;
                            let settings = [
                                (
                                    "Gdk/WindowScalingFactor".to_owned(),
                                    Value::Integer(scale as i32),
                                ),
                                ("Gdk/UnscaledDPI".to_owned(), Value::Integer(dpi)),
                                ("Xft/DPI".to_owned(), Value::Integer(dpi * scale as i32)),
                            ];
                            if let Err(e) = wm.set_xsettings(settings.into_iter()) {
                                // Not fatal, and worth saying loudly: the
                                // screen is already scaled at this point, so
                                // a failure here is the crisp-and-small case
                                // for every client rather than for the few
                                // that cannot scale.
                                tracing::warn!(
                                    "the X settings could not be published, so X11 clients \
                                     will not know to draw at {scale}x: {e}"
                                );
                            }
                        }
                        state.xwm = Some(wm);
                        state.xdisplay = Some(display_number);
                        // Not written into the process environment. This runs
                        // on the event loop with every worker thread — tray,
                        // status, notifications, the bus — already live, and
                        // `setenv` against a concurrent `getenv` is undefined
                        // (see the cursor block in `apply_config`, which used
                        // to do exactly that). Children are handed DISPLAY
                        // explicitly instead: `child_display_env`.
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

    /// The environment a spawned child needs that it cannot inherit.
    ///
    /// DISPLAY, when Xwayland is up. It used to be written into this process's
    /// environment when Xwayland reported ready — `setenv` on a live process,
    /// against every worker thread that might be mid-`getenv`, which is the
    /// hazard the cursor theme reload was stripped of the same way. The child
    /// is told here instead, the way the launcher's token and cursor pair
    /// already are.
    pub fn child_display_env(&self) -> Vec<(String, String)> {
        match self.xdisplay {
            Some(number) => vec![("DISPLAY".to_owned(), format!(":{number}"))],
            None => Vec::new(),
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
        // Broadcast the active configuration to the shell.
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
        let event = Event::Config(Box::new(config));
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
            //
            // Negative is not a size. The runtime message that carries the
            // same setting refuses one (`config.gaps` in apply.rs), and a
            // reload that slipped one past here would forward it unchecked to
            // the shell — so the same refusal happens at the door, naming the
            // key, with that field keeping whatever it had.
            let prior = self.config.gaps.clone().unwrap_or_default();
            let mut gaps = viewport_ipc::event::Gaps {
                inner: file.gaps.inner,
                outer: file.gaps.outer,
                smart: file.gaps.smart,
            };
            if gaps.inner.is_some_and(|v| v < 0) {
                tracing::warn!(
                    "config.gaps.inner {} is negative; keeping the current value",
                    gaps.inner.unwrap()
                );
                gaps.inner = prior.inner;
            }
            if gaps.outer.is_some_and(|v| v < 0) {
                tracing::warn!(
                    "config.gaps.outer {} is negative; keeping the current value",
                    gaps.outer.unwrap()
                );
                gaps.outer = prior.outer;
            }
            self.config.gaps = Some(gaps);
        }
        if file.border != crate::config::BorderConfig::default() {
            // Checked for the same reason the gaps are: a negative radius or
            // width is refused by the runtime message, so it is refused here
            // too, and the field it named keeps what it had.
            let prior = self.config.border.clone().unwrap_or_default();
            let mut border = viewport_ipc::event::Border {
                radius: file.border.radius,
                width: file.border.width,
                smart: file.border.smart,
            };
            if border.radius.is_some_and(|v| v < 0) {
                tracing::warn!(
                    "config.border.radius {} is negative; keeping the current value",
                    border.radius.unwrap()
                );
                border.radius = prior.radius;
            }
            if border.width.is_some_and(|v| v < 0) {
                tracing::warn!(
                    "config.border.width {} is negative; keeping the current value",
                    border.width.unwrap()
                );
                border.width = prior.width;
            }
            self.config.border = Some(border);
        }
        // The clock's locale and format. Forwarded whole rather than field by
        // field, and only when the file names one of them: the shell's own
        // answer to an absent block is not a constant this side could write
        // down — it is whatever locale the engine is running under — so
        // sending a `clock` with three nulls in it would be the compositor
        // overruling that with nothing.
        if file.clock != crate::config::ClockConfig::default() {
            self.config.clock = Some(viewport_ipc::event::Clock {
                locale: file.clock.locale.clone(),
                hour12: file.clock.hour12,
                format: file.clock.format.clone(),
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
        let bar_widgets: Vec<viewport_ipc::event::BarWidget> =
            file.bar_widgets.iter().map(bar_widget_ipc).collect();

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
                        viewport_ipc::event::BarItem::Widget(bar_widget_ipc(w))
                    }
                })
                .collect()
        });

        // Which of those actually get drawn: the override's own widgets when
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
        // One fold over what is drawn, and every sampler knows its job.
        let mut sampled = Sampling::default();
        for widget in &drawn_widgets {
            let costs = widget.sampling();
            sampled.mounts.extend(costs.mounts);
            sampled.volume |= costs.volume;
            sampled.mic |= costs.mic;
            sampled.players |= costs.players;
            sampled.battery |= costs.battery;
        }
        self.status
            .configure(sampled.mounts, sampled.volume, sampled.mic);
        // Following every media player on the session is worth doing only for
        // a bar that draws one, which is the same rule the audio sampling
        // above follows. The battery likewise, on the power worker's own
        // switch.
        self.mpris.set_enabled(sampled.players);
        self.power.set_widget(sampled.battery);
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
            // Carried out here too, and not left for the next hotplug: the
            // block is otherwise applied only where an output arrives, and a
            // reload has no arrival to borrow. On the first load the outputs
            // do not exist yet, so this walks the block and keeps it — which
            // is what the comment on `apply_output_config` describes — and on
            // a reload it is what makes the file the last word again.
            self.apply_output_config();
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
        if let Some(mode) = file.osk.as_deref() {
            match crate::config::parse_osk_mode(mode) {
                Ok(mode) => {
                    self.osk_mode = mode;
                    self.config.osk = mode.as_str().to_owned();
                    // A reload can turn the keyboard off, or take a touch
                    // desk from manual back to auto, while a client's
                    // text-input is still enabled — recomputing right away is
                    // what makes that take effect immediately rather than at
                    // the next commit or focus change. Only half the story:
                    // this can lower a keyboard `osk.wanted` raised, but not
                    // one somebody pinned open by hand with the chord, which
                    // is not this function's to know about. That half is the
                    // shell's, driven by the `osk` field notify_config sends
                    // right after this — see `applyOskMode` in `osk.js`.
                    self.sync_osk_wanted();
                }
                Err(e) => tracing::warn!("{e}; leaving osk as {:?}", self.osk_mode.as_str()),
            }
        }
        if let Some(setting) = file.xwayland.scale.as_ref() {
            match crate::config::parse_xwayland_scale(setting) {
                Ok(scale) => {
                    // Recorded, and acted on only by `start_xwayland`. A
                    // reload that changes this says nothing here on purpose:
                    // the log line belongs where the number is used, and
                    // there is nothing this function could do with a new one
                    // — the X screen's size is fixed when the server starts.
                    self.xwayland_scale = scale;
                }
                Err(e) => tracing::warn!(
                    "{e}; leaving the xwayland scale at {}",
                    self.xwayland_scale.as_str()
                ),
            }
        }
        if let Some(dark) = file.dark_mode {
            self.dark_mode = dark;
            // Running applications change on the portal's signal; without this
            // a reload would move the setting and nothing on screen with it.
            self.appearance.set_dark(dark);
            // And the shell, which draws the switch: the config event carries
            // the scheme so a settings panel can show what it is rather than
            // guess. Set here as well as at startup because a reload is the
            // other way the value moves without anybody pressing the chord.
            self.config.dark_mode = dark;
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

        // How much of a record to keep, on the same terms: applied on every
        // load, so a reload that lowers it drops the oldest entries there and
        // then, and one that sets zero empties the centre rather than leaving
        // what is already in it behind a setting that says it keeps nothing.
        let before = self.notification_history.entries().len();
        self.notification_history.set_limit(
            file.notifications
                .history
                .unwrap_or(crate::notification::DEFAULT_HISTORY),
        );
        if self.notification_history.entries().len() != before {
            self.publish_notification_history();
        }

        // The tray, on unless the file turns it off. Applied on every load, so
        // a reload that flips it claims or releases the bus names then and
        // there rather than at the next restart — the same property the
        // stylesheet and the keybindings have.
        self.tray_enabled = file.tray.unwrap_or(true);
        self.tray.set_enabled(self.tray_enabled);
        // How much the clipboard remembers, or nothing at all. Applied on
        // every load, so a reload that turns it off empties it there and then
        // rather than at the next restart.
        self.clipboard
            .set_limit(file.clipboard_history.unwrap_or(25));

        self.tray.set_icon_theme(
            file.icon_theme
                .clone()
                .unwrap_or_else(|| "hicolor".to_owned()),
        );
        // The same key, for the icons the launcher resolves. The tray keeps
        // its own copy for its worker; this one travels with every query sent
        // to the launcher's scanner.
        self.icon_theme = file
            .icon_theme
            .clone()
            .unwrap_or_else(|| "hicolor".to_owned());
        // Applied on every load, so a reload that changes the theme empties
        // the cache then and there rather than at the next restart — the same
        // property the tray cache has, and the thing the user reaches for when
        // they install icons and want to see them. The cache is the scanner's
        // now, so the emptying is a message rather than a method.
        self.launcher_scan.clear_icons();

        if file.idle != crate::config::IdleConfig::default() {
            self.idle_settings = crate::idle::Settings {
                lock_after: file.idle.lock_after,
                blank_after: file.idle.blank_after,
                lock_command: file.idle.lock_command,
            };
        }

        // The one answer to what locking means, worked out once per config
        // load rather than at each lock. Every path that locks — the idle
        // deadline, the `lock` binding, the lid, the power menu's Lock row —
        // goes through `lock_session`, which reads this and nothing else.
        self.lock_mode =
            crate::lock::Mode::from_command(self.idle_settings.lock_command.as_deref());

        self.lid = match file.lid.as_deref() {
            Some(name) => match crate::power::LidAction::parse(name) {
                Some(action) => action,
                None => {
                    tracing::warn!(
                        "lid: {name:?} is not ignore, lock, blank or suspend; leaving as {:?}",
                        self.lid
                    );
                    self.lid
                }
            },
            None => crate::power::LidAction::default_for(self.idle_settings.lock_command.is_some()),
        };
        self.power
            .set_enabled(self.power.widget() || self.lid != crate::power::LidAction::Ignore);

        // The cursor theme, resolved against what is already loaded rather
        // than round-tripped through the process environment. Writing environ
        // on a process whose tray, status and notification threads are live
        // and reading it is undefined — glibc may free or rehash it under a
        // concurrent getenv — and every reload used to do exactly that, twice,
        // whether anything had changed or not.
        let theme = file
            .cursor
            .theme
            .clone()
            .unwrap_or_else(|| self.cursor_theme.name().to_owned());
        let size = file.cursor.size.unwrap_or(self.cursor_theme.size());
        // Only when one of those two moved, and only then: rebuilding on any
        // change to the block would throw the loaded images away because a
        // reload touched the hide deadline, which has nothing to do with what
        // they look like.
        if theme != self.cursor_theme.name() || size != self.cursor_theme.size() {
            // Built straight from the pair above: `Theme::named` takes the
            // values itself, so the loader never needs environ as a go-between.
            // There was a time this wrote `XCURSOR_THEME` and `XCURSOR_SIZE`
            // into the process environment for the old constructor to read
            // straight back — a setenv on a live process, undefined against
            // every thread that might be mid-getenv, run on every reload that
            // touched the cursor block.
            self.cursor_theme = crate::cursor::Theme::named(theme, size);
            // And what the portal answers, or a toolkit keeps sizing its own
            // cursors from the value it was told when it started — which is a
            // pointer that changes size as it crosses into a window, and a
            // setting that appears not to have been respected at all.
            self.appearance
                .set_cursor(self.cursor_theme.name(), self.cursor_theme.size() as i32);
            // The pointer on screen is still the old image, and the compositor
            // draws on damage: nothing else here is damage.
            self.needs_render = true;
        }
        // The magnifier's step and its ceiling. A reload that lowers the
        // ceiling below where the screen is brings the picture back down to
        // meet it, which is a repaint nothing else here would ask for.
        if self
            .magnifier
            .configure(file.magnify.step, file.magnify.max)
        {
            self.needs_render = true;
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
            //
            // Zero and below are refused rather than handed to
            // `seat.add_keyboard`: a rate of zero is a key that repeats never
            // and a delay of zero is one that never stops, and the runtime
            // message that sets these is checked the same way. The field keeps
            // the default the refused value would have displaced.
            let delay = match keyboard.repeat_delay {
                Some(delay) if delay <= 0 => {
                    tracing::warn!(
                        "keyboard.repeat_delay {delay} is not positive; keeping the default 200"
                    );
                    200
                }
                Some(delay) => delay,
                None => 200,
            };
            let rate = match keyboard.repeat_rate {
                Some(rate) if rate <= 0 => {
                    tracing::warn!(
                        "keyboard.repeat_rate {rate} is not positive; keeping the default 25"
                    );
                    25
                }
                Some(rate) => rate,
                None => 25,
            };
            match self.seat.add_keyboard(xkb, delay, rate) {
                Ok(_) => {
                    // Written down for anything that has to be told what this
                    // desk types in rather than asked to guess — which today
                    // is a libei client, whose keymap is sent to it when its
                    // keyboard device is made. Only on success: a layout that
                    // was refused left the previous keymap in place, and
                    // recording the refused one would send a remote client a
                    // keymap the seat is not using.
                    self.keyboard_config = keyboard.clone();
                    tracing::info!(
                        "keymap {:?}{}, repeat {rate}/s after {delay}ms",
                        keyboard.layout.as_deref().unwrap_or("(default)"),
                        keyboard
                            .variant
                            .as_deref()
                            .map(|v| format!(" {v}"))
                            .unwrap_or_default(),
                    );
                }
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
        // The external menu command, when one is named. Absent — the key left
        // out and the variable unset — is the built-in launcher, which is
        // what `Mod4+d` opens by default now that the shell draws one.
        let menu = file.menu.or_else(|| std::env::var("VIEWPORT_MENU").ok());
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
            None => bindings.extend(crate::binding::defaults(
                &terminal,
                menu.as_deref(),
                &layout,
            )),
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
                // Read, not obeyed: this runs every barrier round, and a lock
                // poisoned once by some other thread's panic would pace no
                // client ever again.
                .map(|timer| timer.lock().unwrap_or_else(|e| e.into_inner()))
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

        // Once any surface has ever been seen holding this state, the answer
        // is yes without looking: the requests that set it are answered inside
        // Smithay, so this walk is the only thing that could notice one
        // arriving, and there is no moment where it leaving is visible either.
        // A monotonic flag rather than a count, for exactly that reason — the
        // cost is a tick that keeps running on a desktop whose fifo client is
        // long gone, against a walk over every window's tree on every commit,
        // which is what this exists to stop paying. See `barrier_ever_armed`.
        if self.barrier_ever_armed.get() {
            return true;
        }

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
        if waiting {
            self.barrier_ever_armed.set(true);
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
    /// everything changed. When the list shrinks the ids beyond it go too:
    /// kept, they would grow by whatever the largest list ever sent was, for
    /// the life of the session.
    ///
    /// `hits` is the subset of them that takes the pointer. Everything the
    /// shell floats does, bar one: see `shell_overlay_hits`.
    pub fn set_shell_overlays(
        &mut self,
        rects: Vec<smithay::utils::Rectangle<i32, Logical>>,
        hits: Vec<smithay::utils::Rectangle<i32, Logical>>,
    ) {
        // A cap rather than trust: the list comes over the control socket, and
        // the render elements it becomes are walked on every frame. A client
        // that sends millions is refused here rather than allowed to grow the
        // element list until the desktop cannot draw.
        if rects.len() > MAX_SHELL_OVERLAYS {
            tracing::warn!(
                "shell.overlay sent {} rectangles; more than the {} allowed, refused",
                rects.len(),
                MAX_SHELL_OVERLAYS
            );
            self.notify(&Event::Error {
                context: "shell.overlay".to_owned(),
                message: format!(
                    "{} rectangles is more than the {} this compositor takes",
                    rects.len(),
                    MAX_SHELL_OVERLAYS
                ),
            });
            return;
        }
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
        // And the other half of "kept by position": ids past the end of the
        // list they belong to are not kept by anything. Shrunk rather than
        // drained-and-reminted, so a list that oscillates in length does not
        // churn new ids — and new full-frame damage — every time it dips.
        self.shell_overlay_ids.truncate(rects.len());
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

    /// Read whatever is on the clipboard now, for the history.
    ///
    /// Called from an idle rather than from the selection handler, because
    /// smithay runs that handler before it stores the new selection: asking
    /// the seat inside it hands back the *previous* client's data. See
    /// `SelectionHandler::new_selection` in `handlers`.
    pub fn capture_clipboard(&mut self, mime: String) {
        use smithay::wayland::selection::data_device::request_data_device_client_selection;

        if !self.clipboard.enabled() {
            return;
        }
        // A pipe: the client fills the write end, a thread reads this one.
        // Both ends run on another process's schedule, which is why neither is
        // touched on the compositor's thread.
        let (read, write) = match smithay::reexports::rustix::pipe::pipe() {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!("no pipe for the clipboard: {e}");
                return;
            }
        };
        if let Err(e) = request_data_device_client_selection(&self.seat, mime, write) {
            // The client that owned it has gone in the meantime, or offered a
            // type it will not actually send. Neither is worth more than a
            // debug line: the next copy is another chance.
            tracing::debug!("could not ask for the selection: {e}");
            return;
        }
        self.clipboard.capture(read);
    }

    /// Tell the shell what the clipboard history holds.
    ///
    /// Sent whole, whenever it changes and whenever a picker asks: it is a
    /// short list drawn in one pass, and a shell that reconciled adds against
    /// removes would be doing bookkeeping to save a message sent when somebody
    /// presses copy.
    pub fn notify_clipboard(&mut self) {
        let entries = self.clipboard.entries().to_vec();
        self.notify(&viewport_ipc::Event::ClipboardHistory { entries });
    }

    /// Offer the entry the history has just moved to the top, with the
    /// compositor as the selection's owner.
    ///
    /// This is the whole point of keeping a history: the application that
    /// copied something may have exited hours ago, and a Wayland selection
    /// lives only as long as the client offering it. Owning it here means the
    /// compositor answers when a client pastes — see `send_selection` in
    /// `handlers`.
    pub fn offer_clipboard(&mut self) {
        use smithay::wayland::selection::data_device::set_data_device_selection;
        let dh = self.display_handle.clone();
        set_data_device_selection(
            &dh,
            &self.seat,
            crate::clipboard::offered_mimes(),
            crate::clipboard::Owner::History,
        );
    }

    /// The largest list the launcher answers with.
    ///
    /// A row is a name and an icon, and a desktop with four hundred
    /// applications is not a list somebody scrolls — it is a filter waiting to
    /// be typed.
    const LAUNCHER_LIMIT: usize = 96;

    /// The applications the launcher can start, filtered, as the shell draws
    /// them.
    ///
    /// Scanned off this thread and cached briefly: keystrokes filter one
    /// snapshot, while a package installed during the session appears on the
    /// next refresh after the short cache deadline. What happens here is the
    /// posting of the question; the answer comes back through the loop like
    /// any other message, to `launcher_apply`. The filter is the shell's text, matched
    /// case-insensitively against the name, what the entry says it is for,
    /// and the command it runs — a binary name typed into the field finds its
    /// entry — and against the app_id a token is minted under; absent is the
    /// whole list.
    ///
    /// The query answers with a generation, the number of queries the
    /// compositor has answered, and `launcher.launch` carries it back: a
    /// launch naming a generation the compositor has moved past is a row from
    /// a list the query that replaced it has not answered yet.
    pub fn launcher_query(&mut self, filter: Option<String>) {
        self.launcher_generation += 1;
        let query = crate::launcher::Query {
            generation: self.launcher_generation,
            filter,
            theme: self.icon_theme.clone(),
            limit: Self::LAUNCHER_LIMIT,
        };
        if self.launcher_scan.online() {
            self.launcher_scan.ask(query);
        } else {
            // No thread to ask. Answered here, then: the blocking path this
            // used to be always, kept for the session where the thread would
            // not start. Its icon resolutions are thrown away when the query
            // ends, which a working scanner never throws away — but a session
            // without the thread is already the degraded one.
            let dirs = crate::launcher::directories();
            let desktop = crate::launcher::current_desktop();
            let desktop: Vec<&str> = desktop.iter().map(String::as_str).collect();
            let mut icons = std::collections::HashMap::new();
            let answer = crate::launcher::answer(&query, &dirs, &desktop, &mut icons);
            self.launcher_apply(answer);
        }
    }

    /// Apply a finished scan: the list a launch will be naming into, and the
    /// rows the shell draws.
    ///
    /// Arrives on the loop from the scanner thread. An answer older than the
    /// newest query is dropped rather than drawn — the keystrokes kept coming
    /// while it was being built, and the shell wants the last word, not the
    /// first. They are answered in order, so this only ever passes over a
    /// list a later query has already superseded.
    pub fn launcher_apply(&mut self, answer: crate::launcher::Answer) {
        if answer.generation != self.launcher_generation {
            return;
        }
        // What `launcher.launch` will be naming an index into.
        self.launcher_list = answer.rows.iter().map(|row| row.app.clone()).collect();
        let apps = answer
            .rows
            .iter()
            .enumerate()
            .map(|(id, row)| viewport_ipc::event::LauncherApp {
                id: id as u32,
                name: row.app.name.clone(),
                icon: row.icon.clone(),
                detail: row.app.detail.clone(),
            })
            .collect();
        self.notify(&viewport_ipc::Event::LauncherList {
            generation: self.launcher_generation,
            apps,
        });
    }

    /// Start the application the picker's highlighted row named.
    ///
    /// The process is handed an xdg-activation token minted for it, so the
    /// window that appears opens focused rather than behind whatever the user
    /// moved on to — the launcher knows where the window is going, because it
    /// is the thing that asked for it, and the token is how it says so.
    ///
    /// `generation` is the list the row came from. The picker sends a query
    /// on every keystroke and does not wait for the answer before it lets the
    /// user press Enter, so the list a row is drawn from may already have
    /// been replaced by the time the launch lands: an `id` from the old list
    /// is almost always in range of the new one, and that is how the wrong
    /// application starts. A launch that names a generation the compositor
    /// has moved past is refused.
    pub fn launcher_launch(&mut self, id: u32, generation: u64) {
        if generation != self.launcher_generation {
            self.notify(&viewport_ipc::Event::Error {
                context: "launcher.launch".to_owned(),
                message: format!("the list {generation} is no longer the one on screen"),
            });
            return;
        }
        let Some(app) = self.launcher_list.get(id as usize).cloned() else {
            // An `id` from a list the next query replaced. The picker is
            // closing either way; the error is for the log and for a script.
            self.notify(&viewport_ipc::Event::Error {
                context: "launcher.launch".to_owned(),
                message: format!("no such application {id}"),
            });
            return;
        };

        // A token nobody presented in a minute is not one an application is
        // still coming back with. Pruned here, on the way out, rather than on
        // a timer the event loop would have to run.
        self.xdg_activation_state
            .retain_tokens(|_, data| data.timestamp.elapsed() < std::time::Duration::from_secs(60));
        let (token, _) = self.xdg_activation_state.create_external_token(Some(
            smithay::wayland::xdg_activation::XdgActivationTokenData {
                app_id: Some(app.app_id.clone()),
                ..Default::default()
            },
        ));
        let token = token.as_str().to_owned();

        // `Terminal=true` is run in the terminal `Mod4+Return` opens, the way
        // an external menu does it: the entry names the program, the session
        // names the window it runs in. The terminal is the session's command
        // line, bare — it may be more than one word, and a quote is what
        // makes the shell look for a binary of the whole line's literal name.
        let command = if app.terminal {
            format!("{} -e {}", self.terminal, app.exec)
        } else {
            app.exec
        };
        // The cursor pair goes with it, as this session draws it now rather
        // than as the environment said when the compositor started: a reload
        // that changed the theme no longer writes environ behind the worker
        // threads' backs, so the child is told here instead of inheriting.
        // DISPLAY with it, for the same reason: Xwayland reports ready long
        // after this process started, and environ is not written then either.
        let mut extra = vec![
            ("XDG_ACTIVATION_TOKEN".to_owned(), token),
            (
                "XCURSOR_THEME".to_owned(),
                self.cursor_theme.name().to_owned(),
            ),
            (
                "XCURSOR_SIZE".to_owned(),
                self.cursor_theme.size().to_string(),
            ),
        ];
        extra.extend(self.child_display_env());
        crate::input::spawn_with_env(&command, &extra);
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
        // A screen arriving or leaving moves windows between lists, even where
        // no rectangle changed: the windows on the monitor that went away are
        // on no list at all now.
        self.sync_foreign_outputs();
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
        // Before the frame is composed from it: the renderer draws in the
        // space's order, so a stack still owed a `restack` here would be a
        // float drawn behind the window it belongs in front of, for as long as
        // that frame is on the screen.
        self.settle();

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
    // A `Window` hashes and compares by its id and by nothing inside it, so
    // the interior mutability clippy sees through the `Arc` cannot move an
    // entry out from under the set.
    #[allow(clippy::mutable_key_type)]
    pub fn restack(&mut self) {
        // The floating windows as a set, looked up once.
        //
        // This used to ask `views.iter().any(...)` for every element in the
        // space, which is every window times every view — and it ran once per
        // `view.layout`, which is once per window per animation frame, so the
        // cost of an animation went up with the cube of the number of windows
        // on the desk.
        let floating: std::collections::HashSet<smithay::desktop::Window> = self
            .views
            .iter()
            .filter(|view| view.floating)
            .map(|view| view.window.clone())
            .collect();

        // Bottom to top, as the space holds them.
        let layers: Vec<Layer> = self
            .space
            .elements()
            .map(|window| Layer::of(window, &floating))
            .collect();

        // Nothing to do, which is the usual answer: the shell resends a
        // rectangle for every window on every frame and the stack it describes
        // is the stack that is already there. Raising each float anyway would
        // be a `Vec` shuffle inside the space per window per frame for a
        // desktop that has not changed shape since it was drawn.
        if is_layered(&layers) {
            return;
        }

        let raise: Vec<smithay::desktop::Window> = self
            .space
            .elements()
            .zip(layers.iter())
            .filter(|(_, layer)| **layer == Layer::Floating)
            .map(|(window, _)| window.clone())
            .collect();
        for window in raise {
            self.space.raise_element(&window, false);
        }

        // Above even those: an X11 menu or tooltip, which places itself and is
        // no view at all. A float raised over an open dropdown is the same bug
        // this function exists to fix, one layer up.
        //
        // Recomputed rather than reused, because the raises above moved the
        // elements the earlier pass was indexed against.
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

    /// Do the work a run of `view.layout` messages left owing.
    ///
    /// The shell sends one `view.layout` per window per animation frame, and
    /// both of these are answers about the desktop as a whole rather than about
    /// the one window the message was for: restacking eight times in a row
    /// gives the same stack the first one did, and asking eight times whether a
    /// video is on a different monitor now gets the same answer eight times.
    /// Doing them here instead costs one of each per batch of messages.
    ///
    /// Called where the answer is about to be *read* rather than on a timer, so
    /// nothing can see a stale stack. See the call sites for the argument that
    /// the list of them is complete.
    pub fn settle(&mut self) {
        if std::mem::take(&mut self.needs_restack) {
            self.restack();
        }
        if std::mem::take(&mut self.needs_colour_notify) {
            self.notify_surface_colour();
        }
        if std::mem::take(&mut self.needs_foreign_outputs) {
            self.sync_foreign_outputs();
        }
    }

    /// Which screens each window is on, to every foreign-toplevel client.
    ///
    /// A taskbar drawn per monitor can only put a window in its own list if it
    /// is told which list that is: `output_enter` and `output_leave` are how
    /// the wlr protocol says a move. Announce-time outputs (see
    /// `handlers/compositor.rs`) cover the window that mapped onto an
    /// arrangement that already existed; this covers everything after — the
    /// shell moving a window between screens, a monitor arriving or leaving,
    /// a workspace switch unmapping the windows that lived on the one that
    /// went.
    ///
    /// Cheap to run often: `set_outputs` diffs against what it last said, so
    /// an unchanged pass sends nothing, and the shell resends every rectangle
    /// on every frame of an animation without the desktop hearing about it
    /// again.
    pub fn sync_foreign_outputs(&mut self) {
        let updates: Vec<(u32, Vec<smithay::output::Output>)> = self
            .views
            .iter()
            .map(|view| {
                let outputs = self.space.outputs_for_element(&view.window);
                (view.id, outputs)
            })
            .collect();
        for (id, outputs) in updates {
            self.foreign_management_state.set_outputs(id, outputs);
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

// Kept in the same module: see the included file.
include!("state/shell_lifecycle.rs");

fn blit_shm(
    buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    pixels: &[u8],
    size: smithay::utils::Size<i32, smithay::utils::Physical>,
    what: &str,
) -> Result<(), String> {
    // The byte counts, worked out before anything is touched. A client names
    // the size it wants copied, and a hostile one names an absurd one; see
    // `shm_blit_layout`, which refuses it here rather than letting it reach
    // the copy — or an allocation upstream.
    let (want, row) = shm_blit_layout(size, pixels.len(), what)?;

    smithay::wayland::shm::with_buffer_contents_mut(buffer, |ptr, len, data| {
        if len < want || data.width < size.w || data.height < size.h {
            return Err(format!(
                "the client's buffer is {}x{} and {what} is {}x{}",
                data.width, data.height, size.w, size.h
            ));
        }
        // Row by row, because the client's stride need not be the packed
        // width — and writing as though it were shears the image.
        let stride = data.stride as usize;
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

/// What one shm blit needs of its pixels: the whole frame, and one row.
///
/// In i64, because the frame's byte count is a product of three numbers that
/// each fit an i32 and their product need not — the old `(w * h * 4)` wrapped
/// in release and panicked in debug, so one client message was the difference
/// between a wrong-sized copy and a dead compositor. Sizes past what a real
/// screen asks for are refused outright, as are empty ones, and the pixels on
/// hand are checked against the count before any indexing could run off the
/// end.
fn shm_blit_layout(
    size: smithay::utils::Size<i32, smithay::utils::Physical>,
    pixels_len: usize,
    what: &str,
) -> Result<(usize, usize), String> {
    let row = size.w as i64 * 4;
    let want = row.checked_mul(size.h as i64);
    if size.w <= 0 || size.h <= 0 || want.is_none_or(|want| want > i32::MAX as i64) {
        return Err(format!(
            "{what} is {}x{}, which is not a size that can be copied",
            size.w, size.h
        ));
    }
    if (pixels_len as i64) < want.unwrap() {
        return Err(format!(
            "the {what} frame is {pixels_len} bytes and {}x{} needs {}",
            size.w,
            size.h,
            want.unwrap()
        ));
    }
    Ok((want.unwrap() as usize, row as usize))
}

/// How much of a window sits on one screen, in square logical pixels.
///
/// The measure the serving output is picked by: the screen holding most of the
/// window is the one that composites it, so a window moved across a boundary
/// changes hands exactly once, when the majority does.
fn cast_overlap(screen: Rectangle<i32, Logical>, window: Rectangle<i32, Logical>) -> i64 {
    screen
        .intersection(window)
        .map(|overlap| overlap.size.w as i64 * overlap.size.h as i64)
        .unwrap_or(0)
}

/// The name of the output that serves a window's casts: most of the window,
/// first past the post on ties, and none when it is on no screen.
fn serving_cast_output<I, S>(outputs: I, window: Rectangle<i32, Logical>) -> Option<String>
where
    I: IntoIterator<Item = (S, Option<Rectangle<i32, Logical>>)>,
    S: AsRef<str>,
{
    let mut best: Option<(i64, String)> = None;
    for (name, screen) in outputs {
        let Some(screen) = screen else { continue };
        let area = cast_overlap(screen, window);
        if area > 0 && best.as_ref().is_none_or(|(top, _)| area > *top) {
            best = Some((area, name.as_ref().to_owned()));
        }
    }
    best.map(|(_, name)| name)
}

/// One configured widget as the shell draws it.
///
/// Written once. The `bar_widgets` additions and the `bar_items` override
/// name the same six widgets, and this mapping used to be spelled out under
/// each — twice that had to agree, on the promise that nothing else would
/// ever add a seventh kind to only one of them.
fn bar_widget_ipc(widget: &crate::config::BarWidgetConfig) -> viewport_ipc::event::BarWidget {
    match widget {
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
        crate::config::BarWidgetConfig::Mpris => viewport_ipc::event::BarWidget::Mpris,
        crate::config::BarWidgetConfig::Battery => viewport_ipc::event::BarWidget::Battery,
    }
}

/// What the background samplers owe one drawn widget.
///
/// The wiring under the bar config re-matched the enum once per consumer —
/// mounts here, volume there, players further down — which was a match per
/// question and a chance each to forget a kind. One capability record
/// instead: a widget says here what it costs to draw, and every sampler
/// folds the same answer.
#[derive(Default)]
struct Sampling {
    /// Mounts to stat. A disk widget's own; several disks, several mounts.
    mounts: Vec<String>,
    /// The default sink, over `wpctl`.
    volume: bool,
    /// The default source, over the same.
    mic: bool,
    /// Every media player on the session, for the mpris widget.
    players: bool,
    /// The battery, from the power worker.
    battery: bool,
}

impl crate::config::BarWidgetConfig {
    /// What has to be sampled for this widget to have numbers to draw.
    ///
    /// Nothing for the weather: the shell fetches that itself, which is why
    /// it is the one widget whose absence costs no worker anything.
    fn sampling(&self) -> Sampling {
        match self {
            crate::config::BarWidgetConfig::Disk { path } => Sampling {
                mounts: vec![path.clone().unwrap_or_else(|| "/".to_owned())],
                ..Sampling::default()
            },
            crate::config::BarWidgetConfig::Weather { .. } => Sampling::default(),
            crate::config::BarWidgetConfig::Volume => Sampling {
                volume: true,
                ..Sampling::default()
            },
            crate::config::BarWidgetConfig::Mic => Sampling {
                mic: true,
                ..Sampling::default()
            },
            crate::config::BarWidgetConfig::Mpris => Sampling {
                players: true,
                ..Sampling::default()
            },
            crate::config::BarWidgetConfig::Battery => Sampling {
                battery: true,
                ..Sampling::default()
            },
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

/// Which of the three bands a window belongs in, bottom to top.
///
/// The whole of the compositor's stacking policy: the shell owns layout, and
/// this is the one ordering rule kept away from it. `Ord` is the policy — a
/// bigger `Layer` sits in front of a smaller one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Layer {
    /// Everything the shell laid out, which is the desktop as such.
    Tiled,
    /// A dialog, a palette, a picture-in-picture: deliberately put in front.
    Floating,
    /// An X11 menu or tooltip, which places itself and is no view at all.
    Override,
}

impl Layer {
    // Hashed by id: see the note on `restack`.
    #[allow(clippy::mutable_key_type)]
    fn of(
        window: &smithay::desktop::Window,
        floating: &std::collections::HashSet<smithay::desktop::Window>,
    ) -> Self {
        if window
            .x11_surface()
            .is_some_and(|x11| x11.is_override_redirect())
        {
            // Override wins over floating for a window that is somehow both,
            // which is where the two passes in `restack` would leave it too:
            // the second pass runs last, so it ends up on top.
            Layer::Override
        } else if floating.contains(window) {
            Layer::Floating
        } else {
            Layer::Tiled
        }
    }
}

/// Whether a stack, bottom to top, is already in the order `restack` wants.
///
/// Which is exactly "never goes back down a band". A stack that passes this is
/// one the raises would leave untouched — they preserve relative order within a
/// band and every band is already above the one below it — so it is safe to
/// skip them, and skipping them is what keeps a still desktop from shuffling
/// its space on every frame the shell draws.
fn is_layered(stack: &[Layer]) -> bool {
    stack.windows(2).all(|pair| pair[0] <= pair[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f64, y: f64) -> Point<f64, Logical> {
        (x, y).into()
    }

    /// A stand-in for whatever a renderer composites into, which is a DMA-BUF
    /// under Vulkan and a renderbuffer under GLES. Only its identity matters
    /// here: the question is whether the same buffer comes back.
    #[derive(Debug, PartialEq, Eq)]
    struct Target(u32);

    fn shape(
        w: i32,
        h: i32,
    ) -> (
        smithay::backend::allocator::Fourcc,
        smithay::utils::Size<i32, smithay::utils::Buffer>,
    ) {
        (smithay::backend::allocator::Fourcc::Xrgb8888, (w, h).into())
    }

    /// The whole point of holding them: a share that reads its frames back
    /// composites into a buffer of a screen's size thirty times a second, and
    /// allocating one per frame is fifteen megabytes of VRAM churned per frame
    /// for a buffer whose shape never changes.
    #[test]
    fn a_capture_of_the_same_shape_gets_the_same_buffer_back() {
        let mut held: Vec<Box<dyn std::any::Any>> = Vec::new();
        let (format, size) = shape(1920, 1080);

        assert!(
            take_scratch::<Target>(&mut held, format, size).is_none(),
            "nothing is held before the first capture"
        );
        keep_scratch(&mut held, format, size, Target(1));

        assert_eq!(
            take_scratch::<Target>(&mut held, format, size),
            Some(Target(1)),
            "the second capture draws into the first one's buffer"
        );
        assert!(
            held.is_empty(),
            "and it is out of the pool while it is being drawn into"
        );
    }

    /// A buffer of the wrong size is not a buffer: the capture would be
    /// composited into something that cannot hold it. A resized source
    /// allocates, which is what the pool is for the frame after.
    #[test]
    fn a_capture_of_another_shape_does_not_take_one_that_will_not_fit() {
        let mut held: Vec<Box<dyn std::any::Any>> = Vec::new();
        let (format, size) = shape(1920, 1080);
        keep_scratch(&mut held, format, size, Target(1));

        let (_, other) = shape(1280, 720);
        assert!(take_scratch::<Target>(&mut held, format, other).is_none());
        assert_eq!(held.len(), 1, "and the one that is held stays held");
    }

    /// Held between frames is not held for ever. A desk whose windows are
    /// resized while a share follows them would otherwise keep a screen's
    /// worth of memory for every size it ever passed through.
    #[test]
    fn the_pool_does_not_grow_past_what_is_being_captured() {
        let mut held: Vec<Box<dyn std::any::Any>> = Vec::new();
        for n in 0..(KEPT_CAPTURE_TARGETS as u32 + 3) {
            let (format, size) = shape(100 + n as i32, 100);
            keep_scratch(&mut held, format, size, Target(n));
        }
        assert_eq!(held.len(), KEPT_CAPTURE_TARGETS);

        // The oldest went, so the first shape has to be allocated again.
        let (format, first) = shape(100, 100);
        assert!(take_scratch::<Target>(&mut held, format, first).is_none());
        // And the newest is still there.
        let (format, last) = shape(100 + KEPT_CAPTURE_TARGETS as i32 + 2, 100);
        assert!(take_scratch::<Target>(&mut held, format, last).is_some());
    }

    /// The stacking policy itself, which is what `Ord` on `Layer` is.
    ///
    /// A float behind a tiled window is not merely hard to see but unreachable,
    /// because the space is what a click is tested against as well as what the
    /// renderer draws from; an X11 menu behind a float is the same fault one
    /// layer up.
    #[test]
    fn a_float_is_in_front_of_the_layout_and_a_menu_in_front_of_both() {
        assert!(Layer::Floating > Layer::Tiled);
        assert!(Layer::Override > Layer::Floating);
    }

    /// What `restack` skips on, so it had better be exactly the stacks the
    /// raises would have left alone.
    #[test]
    fn a_stack_that_never_goes_back_down_is_already_right() {
        assert!(is_layered(&[]));
        assert!(is_layered(&[Layer::Floating]));
        assert!(is_layered(&[Layer::Tiled, Layer::Tiled, Layer::Floating]));
        assert!(is_layered(&[
            Layer::Tiled,
            Layer::Floating,
            Layer::Floating,
            Layer::Override
        ]));
    }

    /// And the ones it must not skip. Each of these is a window that has to
    /// move, and a `restack` that returned early on one of them is the bug this
    /// whole function exists to prevent.
    #[test]
    fn a_stack_that_drops_back_down_has_to_be_restacked() {
        // The float that a newly mapped tiled window landed on top of, which
        // is what `view.layout` does on every frame of an animation.
        assert!(!is_layered(&[Layer::Floating, Layer::Tiled]));
        // A float raised over an open dropdown.
        assert!(!is_layered(&[Layer::Override, Layer::Floating]));
        // And one buried in the middle of a stack that is otherwise fine.
        assert!(!is_layered(&[
            Layer::Tiled,
            Layer::Floating,
            Layer::Tiled,
            Layer::Floating
        ]));
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

    fn screen(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    /// The measure the serving output is picked by, and the thing a straddling
    /// window's share depends on: most of the window wins, so a share drawn on
    /// one screen is skipped on the other instead of composited twice.
    #[test]
    fn a_straddling_window_is_served_by_the_screen_holding_most_of_it() {
        let left = screen(0, 0, 1920, 1080);
        let right = screen(1920, 0, 1920, 1080);
        // Two thirds of it on the left.
        let window = screen(1000, 100, 1800, 800);
        let names = [("left", Some(left)), ("right", Some(right))];
        assert_eq!(serving_cast_output(names, window), Some("left".to_owned()));

        // And when it moves over the boundary far enough, it changes hands —
        // once, which is the whole point of picking one.
        let moved = screen(1500, 100, 1800, 800);
        assert_eq!(serving_cast_output(names, moved), Some("right".to_owned()));
    }

    /// A tie has to go somewhere deterministic, or a window sitting exactly
    /// across two equal halves would change hands frame by frame. First past
    /// the post: the space's order decides, and that order is stable for as
    /// long as the set of monitors is.
    #[test]
    fn an_even_straddle_serves_the_first_screen() {
        let left = screen(0, 0, 1000, 500);
        let right = screen(1000, 0, 1000, 500);
        let window = screen(750, 0, 500, 400); // half on each
        let names = [("left", Some(left)), ("right", Some(right))];
        assert_eq!(serving_cast_output(names, window), Some("left".to_owned()));
        // First past the post, so the same tie in the other order goes the
        // other way — which is still one answer per window per period.
        let swapped = [("right", Some(right)), ("left", Some(left))];
        assert_eq!(
            serving_cast_output(swapped, window),
            Some("right".to_owned())
        );
    }

    /// A window on no screen serves from nowhere, and one that only touches a
    /// screen's edge overlaps nothing and is not served from there either.
    #[test]
    fn a_window_on_no_screen_is_served_by_nothing() {
        let left = screen(0, 0, 1920, 1080);
        let names = [("left", Some(left))];
        // Off every screen entirely.
        assert_eq!(
            serving_cast_output(names, screen(5000, 5000, 100, 100)),
            None
        );
        // Touching the edge is not being on it: zero area, no service.
        assert_eq!(
            serving_cast_output(names, screen(1920, 100, 200, 200)),
            None
        );
        // A screen with no geometry yet (mid-arrival) is simply not a candidate.
        let absent = [("left", None::<Rectangle<i32, Logical>>)];
        assert_eq!(serving_cast_output(absent, screen(10, 10, 50, 50)), None);
    }

    fn size(w: i32, h: i32) -> smithay::utils::Size<i32, Physical> {
        smithay::utils::Size::from((w, h))
    }

    /// The byte counts a shared-memory copy needs, at an ordinary size.
    #[test]
    fn a_blit_of_a_real_frame_names_its_bytes() {
        let (want, row) = shm_blit_layout(size(1920, 1080), 1920 * 1080 * 4, "the copy")
            .expect("a normal frame fits");
        assert_eq!((want, row), (1920 * 1080 * 4, 1920 * 4));
    }

    /// A client message can name any size at all. One that does not fit an
    /// i32 in `w * h * 4` used to wrap in release and panic in debug; either
    /// way it must be refused before anything indexes or allocates.
    #[test]
    fn an_absurd_size_is_refused_rather_than_overflowed() {
        // The largest product there is, which overflowed the old i32 math.
        assert!(shm_blit_layout(size(i32::MAX, i32::MAX), usize::MAX, "the copy").is_err());
        // Fits an i64 but not what this compositor will blit.
        assert!(shm_blit_layout(size(1 << 20, 1 << 20), usize::MAX, "the window").is_err());
        // Empty is not a size. A negative one cannot even be built — Smithay's
        // `Size` refuses it at the constructor, which is its own door.
        assert!(shm_blit_layout(size(0, 100), usize::MAX, "the copy").is_err());
    }

    /// The pixels on hand are counted against what the size asks for, so the
    /// row-by-row indexing below cannot run off the end of the frame.
    #[test]
    fn a_blit_without_enough_pixels_is_refused() {
        assert!(shm_blit_layout(size(1920, 1080), 100, "the copy").is_err());
        // Exactly enough passes, a byte short does not.
        assert!(shm_blit_layout(size(4, 2), 32, "the copy").is_ok());
        assert!(shm_blit_layout(size(4, 2), 31, "the copy").is_err());
    }

    /// Screenshot files are created fail-if-exists, so their names have to be
    /// unique as they are made — two asked for back to back used to share a
    /// millisecond and one lost.
    #[test]
    fn two_screenshots_in_a_row_name_two_files() {
        let first = screenshot_temp_path();
        let second = screenshot_temp_path();
        assert_ne!(first, second);
        // Both live under the private sweep directory, not bare /tmp.
        for path in [&first, &second] {
            assert!(path.starts_with(
                std::env::var_os("XDG_RUNTIME_DIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(std::env::temp_dir)
                    .join("viewport-screenshots")
            ));
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
