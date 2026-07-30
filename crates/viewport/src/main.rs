// SPDX-License-Identifier: GPL-3.0-or-later
//
// Viewport, in Rust. Ports src/main.c.
//
// See docs/RUST-REWRITE.md for what is deliberately absent. The short version:
// there is no web engine yet, so the shell is a flat backdrop and windows are
// placed by whatever speaks to the control socket.

mod appearance;
mod apply;
mod binding;
// Not gated on the web engine: an output composite is worth capturing
// whatever is drawing into it.
mod dump;
mod color_management;
mod config;
mod cursor;
mod focus;
mod foreign_toplevel;
mod gamma;
mod framing;
#[cfg(feature = "wpe")]
mod glib_loop;
mod handlers;
mod hdr;
mod headless;
mod idle;
mod input;
mod ipc;
mod notification;
mod output_management;
mod output_power;
mod pointer;
mod render;
mod screencast;
mod screencopy;
mod session;
#[cfg(feature = "wpe")]
mod shell;
mod state;
mod status;
mod tearing;
mod udev;
mod views;
mod workspace;
mod watchdog;
mod winit;

use anyhow::Result;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use tracing_subscriber::EnvFilter;

use crate::state::ViewportState;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("VIEWPORT_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().collect();
    let socket_path = flag(&args, "--socket").map(std::path::PathBuf::from);
    // No renderer and no window: everything but drawing, for tests and CI.
    let headless = args.iter().any(|a| a == "--headless");
    // The real backend. Without it the compositor runs nested under whatever
    // is already displaying, which is what development wants.
    let drm = args.iter().any(|a| a == "--drm");
    // `--renderer gles` is the same switch as VIEWPORT_RENDERER, for a session
    // started from a display manager where setting an environment variable
    // means editing a unit file. The flag wins, because it is the more
    // deliberate of the two.
    if let Some(which) = flag(&args, "--renderer") {
        unsafe { std::env::set_var("VIEWPORT_RENDERER", which) };
    }

    // The config file, before anything reads a default out of it.
    //
    // A missing default file is ordinary; a missing --config is not, because
    // the user named it. Everything else — a syntax error, a permission
    // problem — stops the compositor either way rather than starting with
    // settings the file was meant to change.
    let explicit = flag(&args, "--config").map(std::path::PathBuf::from);
    let config_path = explicit.clone().or_else(config::default_path);
    let config = match config_path.as_deref().map(config::load) {
        Some(Ok(Some(file))) => {
            tracing::info!("loaded config from {}", config_path.as_ref().unwrap().display());
            file
        }
        Some(Ok(None)) => {
            if explicit.is_some() {
                anyhow::bail!("{}: no such file", config_path.unwrap().display());
            }
            tracing::debug!(
                "no config at {}, using defaults",
                config_path.as_ref().unwrap().display()
            );
            config::File::default()
        }
        Some(Err(e)) => return Err(e),
        None => config::File::default(),
    };

    let mut event_loop: EventLoop<ViewportState> = EventLoop::try_new()?;
    let display: Display<ViewportState> = Display::new()?;

    let mut state = ViewportState::new(&mut event_loop, display, socket_path)?;
    state.apply_config(config);
    // Which backend, when nobody said.
    //
    // A compositor started from a TTY has no display to nest in, and one
    // started inside another session does. Requiring the flag made a plain
    // `viewport` from a TTY fail with "winit backend: Failed to initialize an
    // event loop", which names the backend it should not have chosen rather
    // than the missing flag.
    let nested = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("DISPLAY").is_some();
    let drm = drm || (!headless && !nested);
    if drm {
        tracing::info!("drm backend");
        udev::init(&mut event_loop, &mut state)?;
    } else if headless {
        let width = flag(&args, "--width").and_then(|v| v.parse().ok()).unwrap_or(1920);
        let height = flag(&args, "--height").and_then(|v| v.parse().ok()).unwrap_or(1080);
        headless::init(&mut event_loop, &mut state, width, height)?;
    } else {
        tracing::info!("nested backend, inside an existing session");
        winit::init(&mut event_loop, &mut state)?;
    }

    // Child processes should reach this compositor rather than the host one.
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };

    // What the desktop calls itself, which is how xdg-desktop-portal decides
    // whose implementation to use (`src/main.c:206`).
    //
    // The wlroots portal keys its configuration on this name, and without it
    // the portal finds no ScreenCast implementation at all: screen sharing
    // then fails everywhere it is offered — Firefox and Chromium hand back an
    // empty source list, OBS shows no capture — and nothing logs anything
    // that points at a desktop name. The capture protocols were never the
    // missing part; this string is.
    //
    // Not overwritten: a session that set it already has said what it wants to
    // be called.
    if std::env::var_os("XDG_CURRENT_DESKTOP").is_none() {
        unsafe { std::env::set_var("XDG_CURRENT_DESKTOP", "viewport:wlroots") };
    }
    // What kind of session this is, overwriting whatever logind decided.
    //
    // A compositor started from a TTY inherits XDG_SESSION_TYPE=tty, which is
    // true of the login and false of everything running inside it. Firefox
    // reads it to decide whether screen sharing goes through the portal, and
    // with "tty" it falls back to capturing the X11 root window through
    // Xwayland — which has no contents. That is a share of a black rectangle
    // at the right resolution with a cursor drawn on it, and the portal is
    // never called at all, so nothing anywhere logs a reason.
    //
    // sway sets it the same way, unconditionally.
    unsafe { std::env::set_var("XDG_SESSION_TYPE", "wayland") };

    // Only the session's own compositor, which is the one on DRM.
    //
    // A nested or headless instance is a client of the session around it, and
    // exporting its socket to the user manager takes the portal — and anything
    // else D-Bus activates — away from the compositor the user is actually
    // looking at. Every test run left the session pointing at a socket that
    // no longer existed, and the portal then failed with "failed to connect to
    // display" until someone put it back by hand.
    if drm {
        export_session_environment();
    }

    // Before anything is spawned, so an X program started from a menu finds a
    // DISPLAY. It arrives asynchronously; the variable is set when it does.
    state.start_xwayland(&event_loop.handle());

    tracing::info!(
        "viewport {} on {} (smithay rewrite)",
        env!("CARGO_PKG_VERSION"),
        state.socket_name.to_string_lossy()
    );
    // Whether there is a shell in this binary at all.
    //
    // The feature is not the default, so `cargo test` and a plain
    // `cargo build` both leave a binary with no web engine in
    // target/release — and run-drm.sh runs whatever is there. A session with
    // no shell looks exactly like a shell that failed to paint: grey where
    // the wallpaper and the bar should be, and nothing in the log to say
    // which, because the code that would have logged is not compiled in.
    if cfg!(feature = "wpe") {
        tracing::info!("the shell is compiled in");
    } else {
        tracing::warn!(
            "no shell in this binary: rebuild with \
             `cargo build --release -p viewport --features wpe`"
        );
    }

    // A self-imposed deadline, for trying things on a real TTY.
    //
    // Every other way out depends on something working: the quit chord needs
    // input routing, and the control socket needs another terminal. This needs
    // only the event loop, so a run that comes up wrong still ends by itself
    // rather than holding the machine.
    if let Some(seconds) = flag(&args, "--exit-after").and_then(|v| v.parse::<u64>().ok()) {
        tracing::info!("will exit after {seconds}s");
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(
            std::time::Duration::from_secs(seconds),
        );
        event_loop
            .handle()
            .insert_source(timer, |_, _, state| {
                tracing::info!("the --exit-after deadline passed; stopping");
                state.loop_signal.stop();
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            })
            .map_err(|e| anyhow::anyhow!("inserting the exit timer: {e}"))?;
    }

    // The shell's first-paint deadline. Zero turns it off, which a shell being
    // developed wants — a page that is slow to come up is not a failure then.
    #[cfg(feature = "wpe")]
    if state.load_timeout_ms > 0 {
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(
            std::time::Duration::from_millis(state.load_timeout_ms),
        );
        event_loop
            .handle()
            .insert_source(timer, |_, _, state| {
                state.check_shell_loaded();
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            })
            .map_err(|e| anyhow::anyhow!("inserting the shell load timer: {e}"))?;
    }

    // One slow tick. A second is fine: the idle deadlines are in seconds, and
    // the lock check is only there to notice a locker that never drew.
    //
    // Always inserted, even with no idle deadlines configured — the lock check
    // is not optional, because it is the only thing that says anything at all
    // when a locked screen is black and eating every key.
    {
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(
            std::time::Duration::from_secs(1),
        );
        event_loop
            .handle()
            .insert_source(timer, |_, _, state| {
                state.idle_tick();
                state.check_lock_screen();
                smithay::reexports::calloop::timer::TimeoutAction::ToDuration(
                    std::time::Duration::from_secs(1),
                )
            })
            .map_err(|e| anyhow::anyhow!("inserting the housekeeping timer: {e}"))?;
    }

    // Notifications, over D-Bus, forwarded to the shell.
    //
    // On a thread of its own with a channel back: zbus wants an async runtime
    // and this loop is GLib with calloop inside it, and making three schedulers
    // agree is worse than one channel.
    {
        let (sender, source) = smithay::reexports::calloop::channel::channel();
        event_loop
            .handle()
            .insert_source(source, |event, _, state| {
                use smithay::reexports::calloop::channel::Event;
                let Event::Msg(message) = event else {
                    return;
                };
                match message {
                    crate::notification::Message::Add(notification) => {
                        state.notify(&viewport_ipc::Event::NotificationAdd(*notification));
                    }
                    crate::notification::Message::Close(id) => {
                        state.notify(&viewport_ipc::Event::NotificationClose { id });
                    }
                }
            })
            .map_err(|e| anyhow::anyhow!("inserting the notification source: {e}"))?;

        // Not fatal: a session with no D-Bus, or one where mako already holds
        // the name, still has a working compositor.
        if let Err(e) = state.notifications.start(sender) {
            tracing::warn!("notifications are unavailable: {e}");
        }
    }

    // The portals this compositor answers itself: dark mode, and screen
    // sharing. Both on one connection, because they share a bus name — a
    // second connection claiming it does not get it, and the interface built
    // second is simply missing from the bus.
    {
        let (sender, source) = smithay::reexports::calloop::channel::channel();
        event_loop
            .handle()
            .insert_source(source, |event, _, state| {
                use smithay::reexports::calloop::channel::Event;
                let Event::Msg(message) = event else {
                    return;
                };
                state.handle_screencast(message);
            })
            .map_err(|e| anyhow::anyhow!("inserting the screencast source: {e}"))?;

        let settings = crate::appearance::Settings {
            color_scheme: if state.dark_mode {
                crate::appearance::PREFER_DARK
            } else {
                crate::appearance::PREFER_LIGHT
            },
            // The cursor the compositor actually draws, so a toolkit does not
            // size its own differently from the pointer on screen.
            cursor_theme: state.cursor_theme.name().to_owned(),
            cursor_size: state.cursor_theme.size() as i32,
        };
        let screencast = crate::screencast::portal::ScreenCast::new(sender);

        // Not fatal: a real desktop portal already holding the name knows more
        // about the session than this does, and applications keep the defaults
        // they had a moment ago.
        if let Err(e) = state.appearance.start(settings, screencast) {
            tracing::warn!("the portals are unavailable: {e}");
        }
    }

    // System statistics for the bar. Every two seconds, as in C
    // (`src/status.c:236`): the numbers are rates and averages, and sampling
    // faster only makes them noisier.
    {
        let period = std::time::Duration::from_secs(2);
        let timer = smithay::reexports::calloop::timer::Timer::from_duration(period);
        event_loop
            .handle()
            .insert_source(timer, move |_, _, state| {
                state.status_tick();
                smithay::reexports::calloop::timer::TimeoutAction::ToDuration(period)
            })
            .map_err(|e| anyhow::anyhow!("inserting the status timer: {e}"))?;
    }

    // Whatever the config file asked to be run, once everything it needs is
    // in the environment: WAYLAND_DISPLAY, DISPLAY, and the outputs.
    if let Some(command) = state.startup.clone() {
        tracing::info!("startup: {command}");
        input::spawn(&command);
    }

    // With the web engine, GLib owns the outer loop and calloop nests inside
    // it — see glib_loop.rs for why round that way.
    #[cfg(feature = "wpe")]
    {
        let mut glib = glib_loop::GlibLoop::new(&event_loop)?;
        state.glib = Some(glib.signal());
        glib.run(&mut event_loop, &mut state);
        return Ok(());
    }

    #[cfg(not(feature = "wpe"))]
    {
        event_loop.run(None, &mut state, |_| {})?;
    }
    Ok(())
}

/// The value after `name` on the command line.
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let at = args.iter().position(|a| a == name)?;
    args.get(at + 1).map(String::as_str)
}

/// Hand the session's environment to the user services, which this compositor
/// does not start and cannot reach any other way.
///
/// xdg-desktop-portal and its backends are D-Bus activated with whatever
/// environment the user manager holds, and xdg-desktop-portal-wlr guards
/// itself with
///
///   ConditionEnvironment=WAYLAND_DISPLAY
///
/// so with nothing exported it is skipped before it runs a line. Everything
/// downstream of that fails quietly: ScreenCast reports no sources, a browser
/// offers a picker of black rectangles, OBS shows no screen capture — and the
/// compositor is never asked for a frame, so its own log says nothing at all.
/// The journal says "skipped, unmet condition check", which is accurate and
/// names nothing anyone would search for (`src/server.c:411`).
///
/// Both commands, because which one is authoritative depends on the system,
/// and failure is fine: a session with neither systemd nor D-Bus wants no part
/// of this and works regardless.
fn export_session_environment() {
    const VARIABLES: [&str; 3] =
        ["WAYLAND_DISPLAY", "XDG_CURRENT_DESKTOP", "XDG_SESSION_TYPE"];

    let commands: [(&str, Vec<&str>); 2] = [
        (
            "systemctl",
            ["--user", "import-environment"]
                .into_iter()
                .chain(VARIABLES)
                .collect(),
        ),
        (
            "dbus-update-activation-environment",
            ["--systemd"].into_iter().chain(VARIABLES).collect(),
        ),
    ];

    for (program, arguments) in commands {
        match std::process::Command::new(program).args(&arguments).spawn() {
            // Reaped on a thread of its own: both exit at once, and a
            // compositor that never waits would leave two zombies for the life
            // of the session.
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => tracing::debug!("could not run {program}: {e}"),
        }
    }
}
