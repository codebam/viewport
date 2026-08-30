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
    /// Per-device settings retained for devices hotplugged after config load.
    pub input_config: std::collections::HashMap<String, crate::config::InputConfig>,
    /// How each monitor was arranged when it was last seen, by connector name.
    ///
    /// A connector that comes back is a new output as far as the backend is
    /// concerned: it is placed to the right of everything else, in whatever
    /// order the connectors are enumerated, turned the way it left the factory.
    /// Two monitors switched off overnight therefore came back swapped, and a
    /// rotated one came back landscape. This is what they go back to.
    pub output_memory: std::collections::HashMap<String, RememberedOutput>,
    /// Physical mirror sinks keyed by sink connector, naming direct sources.
    pub output_mirrors: std::collections::HashMap<String, String>,
    /// Explicit per-head VRR policies. Heads absent here use the legacy global
    /// `adaptive_sync` default.
    pub output_vrr: std::collections::HashMap<String, viewport_ipc::event::VrrMode>,
    /// Last state successfully requested from KMS, for publication and to
    /// avoid programming VRR on unrelated output frames.
    pub output_vrr_effective: std::collections::HashMap<String, bool>,
    pub output_vrr_wanted: std::collections::HashMap<String, bool>,
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
    /// AI subscription limits and OpenRouter credits. Idle unless an AI widget
    /// has a usable credential.
    pub ai_usage: crate::ai_usage::AiUsage,
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
    /// Configured discrete gestures and the captured sequence in progress.
    pub gestures: Vec<crate::input::GestureBinding>,
    pub gesture: Option<crate::input::GestureState>,

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
        let xdg_shell_state = XdgShellState::new_with_capabilities::<Self>(
            &dh,
            [
                smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::WmCapabilities::Fullscreen,
                smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::WmCapabilities::Maximize,
            ],
        );
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
        let direct_capture_allowed = |client: &smithay::reexports::wayland_server::Client| {
            client
                .get_data::<ClientState>()
                .is_none_or(|data| data.security_context.is_none())
        };
        let output_capture_source_state =
            smithay::wayland::image_capture_source::OutputCaptureSourceState::new_with_filter::<
                Self,
                _,
            >(&dh, direct_capture_allowed);
        // Windows as well as screens. The picker in a browser's "share your
        // screen" dialogue lists both, and a client that binds this manager
        // and finds nothing behind it has no way to offer the second.
        let toplevel_capture_source_state =
            smithay::wayland::image_capture_source::ToplevelCaptureSourceState::new_with_filter::<
                Self,
                _,
            >(&dh, direct_capture_allowed);
        let image_copy_capture_state =
            smithay::wayland::image_copy_capture::ImageCopyCaptureState::new_with_filter::<Self, _>(
                &dh,
                direct_capture_allowed,
            );
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
                layout_extensions: Vec::new(),
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
                workspaces: Vec::new(),
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
            input_config: std::collections::HashMap::new(),
            output_memory: std::collections::HashMap::new(),
            output_mirrors: std::collections::HashMap::new(),
            output_vrr: std::collections::HashMap::new(),
            output_vrr_effective: std::collections::HashMap::new(),
            output_vrr_wanted: std::collections::HashMap::new(),
            startup: None,
            notifications: crate::notification::Notifications::default(),
            notification_history: crate::notification::History::default(),
            tray: crate::tray::Tray::default(),
            mpris: crate::mpris::Mpris::default(),
            ai_usage: crate::ai_usage::AiUsage::default(),
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
            gestures: Vec::new(),
            gesture: None,
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

include!("state/outputs.rs");

impl ViewportState {
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

include!("state/output_controls.rs");

impl ViewportState {
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
        let time = smithay::backend::input::InputTime::now();
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
        self.status_tick_with_osd(None);
    }

    pub fn status_tick_with_osd(&mut self, osd: Option<viewport_ipc::event::StatusOsd>) {
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
            brightness: sample.brightness.unwrap_or(-1.0),
            osd,
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
}

include!("state/lock_power.rs");

impl ViewportState {
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
    /// The restore is replayed rather than made provisional: after DPMS wake,
    /// DisplayPort connectors can return one at a time, and a revert snapshot
    /// taken between them describes that transient one-monitor desk. Reverting
    /// to it twelve seconds later removes the second logical desktop and makes
    /// the two physical heads appear mirrored.
    /// Run before [`Self::apply_output_config`], so a file that names a
    /// position still has the last word.
    pub fn restore_output_layout(&mut self) {
        let was_replay = std::mem::replace(&mut self.output_config_replay, true);
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
                vrr: None,
                mirror: None,
                x: Some(want.x),
                y: Some(want.y),
            };
            crate::apply::apply(self, viewport_ipc::Request::OutputConfigure(request));
        }
        self.output_config_replay = was_replay;
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
            if self.any_output_by_name(name).is_none() {
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
                vrr: want.vrr,
                mirror: None,
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
        // Topology after every head's own mode/scale/transform. HashMap order
        // must not decide whether two eventually matching heads compare equal.
        for (name, want) in &outputs {
            let Some(source) = want.mirror.clone() else {
                continue;
            };
            if self.any_output_by_name(name).is_none() {
                continue;
            }
            crate::apply::apply(
                self,
                viewport_ipc::Request::OutputConfigure(viewport_ipc::request::OutputConfigure {
                    name: name.clone(),
                    enabled: None,
                    mode: None,
                    scale: None,
                    transform: None,
                    adaptive_sync: None,
                    vrr: None,
                    mirror: Some(source),
                    x: None,
                    y: None,
                }),
            );
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
}

include!("state/render_frame.rs");

impl ViewportState {
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
        self.udev
            .as_ref()
            .and_then(|udev| {
                udev.surfaces()
                    .find(|surface| surface.output.name() == name)
                    .map(|surface| surface.output.clone())
            })
            .or_else(|| {
                self.headless
                    .as_ref()
                    .and_then(|headless| headless.outputs.get(name).cloned())
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
        let events: Vec<(Event, bool)> = self
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
                (
                    Event::ViewAdded(v.added(
                        output,
                        true,
                        self.views.parent_id_of(v),
                        self.views.ancestor_ids_of(v),
                    )),
                    v.wants_fullscreen(),
                )
            })
            .collect();
        for (event, fullscreen) in events {
            let id = match &event {
                Event::ViewAdded(view) => view.id,
                _ => unreachable!("notify_views only builds view.added events"),
            };
            self.notify(&event);
            if fullscreen {
                self.notify_fullscreen(id, true);
            }
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
}

include!("state/config_apply.rs");

impl ViewportState {}

include!("state/frame_barriers.rs");

impl ViewportState {}

include!("state/clipboard_launcher.rs");

impl ViewportState {
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
        self.physical_outputs()
            .into_iter()
            .filter(|output| {
                let scene = self.mirror_source(output);
                match (region, self.space.output_geometry(&scene)) {
                    (Some(region), Some(geometry)) => geometry.overlaps(region),
                    (Some(_), None) => true,
                    _ => true,
                }
            })
            .map(|output| {
                let scene = self.mirror_source(&output);
                let geometry = self.space.output_geometry(&scene).unwrap_or_else(|| {
                    let size = output
                        .current_mode()
                        .map(|mode| output.current_transform().transform_size(mode.size))
                        .map(|size| {
                            let scale = output.current_scale().fractional_scale();
                            (
                                (f64::from(size.w) / scale).round() as i32,
                                (f64::from(size.h) / scale).round() as i32,
                            )
                                .into()
                        })
                        .unwrap_or_default();
                    Rectangle::new(Point::default(), size)
                });
                let usable = self
                    .space
                    .output_geometry(&scene)
                    .map(|_| self.usable_area(&scene))
                    .unwrap_or(geometry);
                let props = output.physical_properties();
                let current = output.current_mode();
                let physical_size = props.size;
                let physical_dimensions = (physical_size.w > 0 && physical_size.h > 0)
                    .then_some((physical_size.w, physical_size.h));
                OutputInfo {
                    name: output.name(),
                    // Never null: the shell concatenates these without
                    // guarding (`src/ipc.c:704`).
                    make: props.make,
                    model: props.model,
                    serial: String::new(),
                    physical_width_mm: physical_dimensions.map(|size| size.0),
                    physical_height_mm: physical_dimensions.map(|size| size.1),
                    enabled: self.output_is_enabled(&output),
                    role: if self.output_mirrors.contains_key(&output.name()) {
                        viewport_ipc::event::OutputRole::MirrorSink
                    } else if self
                        .output_mirrors
                        .values()
                        .any(|source| source == &output.name())
                    {
                        viewport_ipc::event::OutputRole::MirrorSource
                    } else {
                        viewport_ipc::event::OutputRole::Desktop
                    },
                    mirror_source: self.output_mirrors.get(&output.name()).cloned(),
                    vrr: self.configured_vrr(&output.name()),
                    vrr_effective: *self
                        .output_vrr_effective
                        .get(&output.name())
                        .unwrap_or(&false),
                    // The shell owns this — it tracks the pointer and keyboard
                    // focus, and tells the compositor. Reporting it back is
                    // what lets anything else ask which screen the user is on:
                    // a screenshot tool otherwise has to guess, and guessing
                    // over two monitors means capturing both.
                    active: self.active_output.as_deref() == Some(output.name().as_str())
                        && !self.output_mirrors.contains_key(&output.name()),
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
}

include!("state/frame_clock.rs");

impl ViewportState {
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
            let maximized = self.view_is_maximized(previous);
            let minimized = self.view_is_minimized(previous);
            self.foreign_management_state
                .set_state(previous, false, maximized, minimized, fullscreen);
        }
        let fullscreen = self.view_is_fullscreen(id);
        let maximized = self.view_is_maximized(id);
        let minimized = self.view_is_minimized(id);
        self.foreign_management_state
            .set_state(id, !minimized, maximized, minimized, fullscreen);

        let event = Event::ViewFocused { id };
        self.notify(&event);
    }

    /// Whether a window is fullscreen, as the state it was configured with
    /// says — the shell decides it, and this is where it landed.
    pub(crate) fn view_is_fullscreen(&self, id: u32) -> bool {
        self.views
            .get(id)
            .map(crate::views::View::wants_fullscreen)
            .unwrap_or(false)
    }

    pub(crate) fn view_is_maximized(&self, id: u32) -> bool {
        self.views
            .get(id)
            .map(crate::views::View::wants_maximized)
            .unwrap_or(false)
    }

    pub(crate) fn view_is_minimized(&self, id: u32) -> bool {
        self.views
            .get(id)
            .map(|view| view.minimized)
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
        crate::config::BarWidgetConfig::Ai { provider, .. } => viewport_ipc::event::BarWidget::Ai {
            provider: provider.name().to_owned(),
        },
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
            crate::config::BarWidgetConfig::Ai { .. } => Sampling::default(),
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
    /// Exact xdg-shell edge name for client-driven resizes. Mod4 resizes infer
    /// a corner from `edges` instead.
    pub edge: Option<&'static str>,
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
    /// True when xdg-shell or XWayland started the gesture from a client. Its
    /// pointer grab owns the release; Mod4 drags are ended directly by the
    /// input path instead.
    pub client_requested: bool,
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
