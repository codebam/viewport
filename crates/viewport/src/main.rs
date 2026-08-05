// SPDX-License-Identifier: GPL-3.0-or-later
//
// Viewport, in Rust. Ports src/main.c.
//
// See docs/RUST-REWRITE.md for what is deliberately absent. The short version:
// there is no web engine yet, so the shell is a flat backdrop and windows are
// placed by whatever speaks to the control socket.

mod appearance;
mod apply;
mod background;
mod binding;
mod capture;
// Not gated on the web engine: an output composite is worth capturing
// whatever is drawing into it.
mod color_management;
mod config;
mod cursor;
mod dump;
mod focus;
mod foreign_toplevel;
mod framing;
mod gamma;
mod handlers;
mod hdr;
mod headless;
mod idle;
mod input;
mod ipc;
mod msg;
mod notification;
mod output_management;
mod output_power;
mod pointer;
mod recovery;
mod render;
mod screencast;
mod screencopy;
mod session;
#[cfg(feature = "wpe")]
mod shell;
mod shell_backend;
mod shell_client;
mod shell_watch;
mod state;
mod status;
mod tearing;
mod udev;
mod views;
mod watchdog;
mod winit;
mod workspace;

use anyhow::Result;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use tracing_subscriber::EnvFilter;

use crate::state::ViewportState;

/// Run the compositor, then leave without running C's exit handlers.
///
/// Everything this process owns is dropped by `run` returning: the DRM state
/// is restored, libseat gives the devices back, and the shell thread is asked
/// to stop and joined. Rust destructors are what do all of that, and they run
/// before this function is reached.
///
/// What is left after them is the C library's own teardown, and under the web
/// engine that teardown aborts. WebKit builds a default `WebKitNetworkSession`
/// on whichever thread first makes a web view — the shell thread here — and
/// registers it to be destroyed at exit. glibc then runs that from the main
/// thread, `~WebsiteDataStore` finds itself on the wrong one, and
/// `WTFCrashWithInfo` calls `abort`. Every session on this machine ended that
/// way: eight cores, all of them raised from `__run_exit_handlers`, all after
/// the compositor had finished and given the screens back.
///
/// So the process leaves before those handlers run. `_exit` skips them and
/// skips stdio flushing with them, which is why the log is flushed here by
/// hand rather than left to `exit`.
///
/// The alternative — building the session explicitly on the shell thread and
/// unreffing it there, so the default is never made — fixes this one global
/// and not the next. WebKit registers several.
fn main() -> ! {
    // `viewport msg`, before any of the above applies: it talks to a
    // compositor that is already running rather than starting one, so it wants
    // no backend, no seat and no log — and no exit path that skips flushing
    // the answer it just printed.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|first| first == "msg") {
        let code = msg::main(&args[2..]);
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        std::process::exit(code);
    }

    // Before the log is set up and before a backend is chosen: asking what the
    // options are should not need a seat, and should not print a line of
    // tracing above the answer.
    if args
        .iter()
        .skip(1)
        .any(|arg| arg == "--help" || arg == "-h")
    {
        print_help();
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        std::process::exit(0);
    }

    let code = match run() {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("{e:#}");
            1
        }
    };
    use std::io::Write as _;
    let _ = std::io::stderr().flush();
    let _ = std::io::stdout().flush();
    // SAFETY: nothing of this process's own is left to clean up — `run` has
    // returned and its destructors with it. This only skips what the C library
    // would do next.
    unsafe { libc::_exit(code) }
}

fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("VIEWPORT_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().collect();
    warn_about_unknown_options(&args);
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
    let mut config = match config_path.as_deref().map(config::load) {
        Some(Ok(Some(file))) => {
            tracing::info!(
                "loaded config from {}",
                config_path.as_ref().unwrap().display()
            );
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

    // `--layout solar` is the same switch as the config file's "layout", for
    // trying one out without editing anything. Written over the file's value
    // rather than applied after it, because the keymap is built from this: a
    // few chords exist only in one model, and apply_config puts the bindings
    // together last precisely so it can read the answer. Set afterwards, the
    // layout would change and its keys would not.
    //
    // Not validated here. apply_config rejects an unknown name and says which
    // ones it knows, and one message about a bad layout is better than two.
    if let Some(layout) = flag(&args, "--layout") {
        tracing::info!("layout from the command line: {layout}");
        config.layout = Some(layout.to_owned());
    }

    let mut event_loop: EventLoop<ViewportState> = EventLoop::try_new()?;
    let display: Display<ViewportState> = Display::new()?;

    let mut state = ViewportState::new(&mut event_loop, display, socket_path)?;

    // Before the config, because the config file is only consulted where
    // neither the command line nor the environment said anything. A name that
    // cannot be honoured is reported and fallen back from rather than being
    // fatal: this is the setting that decides whether there is a desktop, and
    // refusing to start over it leaves nothing to log in to and fix it with.
    {
        let asked = flag(&args, "--shell-backend");
        state.shell_backend = shell_backend::choose(asked, None);
        state.shell_backend_from_flag =
            asked.is_some() || std::env::var_os("VIEWPORT_SHELL_BACKEND").is_some();
    }

    state.apply_config(config);
    // After the config, so the flag wins — the same rule `--renderer` follows,
    // and for the same reason: naming it on the command line is the more
    // deliberate of the two. Failing here rather than falling back, because a
    // shell that was asked for by name and cannot be loaded is a mistake to
    // report, not a reason to quietly start a different one.
    if let Some(url) = flag(&args, "--url") {
        let resolved = config::shell_url(url)?;
        tracing::info!("shell url from the command line: {resolved}");
        state.shell_url = Some(resolved);
    }
    // Whether that page takes every monitor or only the first.
    //
    // Only the first by default, with the shipped desktop on the rest: `--url
    // https://example.com` means "that site on my main monitor", and a session
    // with no window manager anywhere is not what anybody asked for by naming a
    // web page. A shell under development is the other case — it *is* the
    // desktop — and this is how it says so.
    if args.iter().any(|a| a == "--url-span") {
        state.shell_url_spans = true;
    }

    // Watching the shell's own files, if asked. After the URL, because the URL
    // is what says which directory to watch, and before any of it is loaded,
    // because a change between here and the first paint should still count.
    if shell_watch::wanted(&args) {
        state.watch_shell_assets();
    }

    // A terminal for a wallpaper, from the command line.
    //
    // Bare `--background-terminal` runs the configured terminal;
    // `--background-terminal='foot -e btop'` runs that instead, which is the
    // form worth typing — the wallpaper is never given input, so a login shell
    // in it sits at a prompt forever and a program in it is the point.
    //
    // After the config, so the flag wins, and both forms are handled here
    // because `flag` reads the next argument as a value and a bare switch has
    // none: `--background-terminal --drm` would otherwise run `--drm`.
    if let Some(command) = flag(&args, "--background-terminal")
        .filter(|value| !value.starts_with("--"))
        .filter(|value| !value.trim().is_empty())
    {
        tracing::info!("background terminal from the command line: {command}");
        state.background_command = Some(command.to_owned());
        state.config.background_terminal = true;
    } else if args.iter().any(|arg| arg == "--background-terminal") {
        let terminal = state.terminal.clone();
        tracing::info!("background terminal from the command line: {terminal}");
        state.background_command = Some(terminal);
        state.config.background_terminal = true;
    }

    // Which backend, when nobody said.
    //
    // A compositor started from a TTY has no display to nest in, and one
    // started inside another session does. Requiring the flag made a plain
    // `viewport` from a TTY fail with "winit backend: Failed to initialize an
    // event loop", which names the backend it should not have chosen rather
    // than the missing flag.
    let nested =
        std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    let drm = drm || (!headless && !nested);

    // A shell with no display to nest in is not the same thing as a shell
    // sitting at a seat, and until now this could not tell the difference.
    //
    // A terminal that belongs to no session — a tmux started by the service
    // manager, a systemd unit, anything reattached after its login went away —
    // has neither `WAYLAND_DISPLAY` nor `XDG_VTNR`. This chose DRM for it,
    // libseat handed over the seat's *active* session because that is the one
    // asking, and the compositor took the screen out from under the desktop
    // already running on it. Which is a spectacular way to find out, and it
    // happened three times in one evening.
    //
    // So: DRM needs either a VT of its own or an explicit `--drm`. The flag
    // is what says "I mean it", and it is what run-drm.sh and the benchmark
    // harness already pass.
    if drm && !args.iter().any(|arg| arg == "--drm") && std::env::var_os("XDG_VTNR").is_none() {
        anyhow::bail!(
            "no display to nest in and no VT of this session's own: WAYLAND_DISPLAY, \
             DISPLAY and XDG_VTNR are all unset. Taking DRM from here would take the \
             screen from whatever is already on this seat. Run this from a TTY, or set \
             WAYLAND_DISPLAY to nest inside a session, or pass --drm to mean it"
        );
    }

    if drm {
        tracing::info!("drm backend");
        udev::init(&mut event_loop, &mut state)?;
    } else if headless {
        let width = flag(&args, "--width")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1920);
        let height = flag(&args, "--height")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1080);
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

    // `WAYLAND_DISPLAY=<name>` spelled out, not just the name: the socket is
    // chosen by libwayland and there is no way to ask for a particular one, so
    // the integration tests in tests/ start the compositor and grep this line
    // out of its log to find out where to connect. The C build prints it in
    // the same shape, which is what lets tests/capture.test.sh and
    // tests/lock.test.sh run against either binary unchanged — and that is the
    // form parity has to take before src/ can be deleted.
    tracing::info!(
        "viewport {} on WAYLAND_DISPLAY={} (smithay rewrite)",
        env!("CARGO_PKG_VERSION"),
        state.socket_name.to_string_lossy()
    );
    // Which engine is going to draw the desktop, said once and plainly.
    //
    // A session with no shell looks exactly like a shell that failed to paint:
    // grey where the wallpaper and the bar should be, and nothing in the log
    // to say which. That used to be the common case — the `wpe` feature is not
    // the default, so a plain `cargo build` left a binary with no engine in it
    // at all — and it is why this line exists.
    match state.shell_backend {
        shell_backend::ShellBackend::Wpe => {
            tracing::info!("shell backend: wpe, the engine in this process");
        }
        backend if backend.is_out_of_process() => {
            tracing::info!("shell backend: {backend}, in a process of its own");
        }
        backend => {
            tracing::error!("shell backend: {backend}, which cannot draw anything");
        }
    }

    // A self-imposed deadline, for trying things on a real TTY.
    //
    // Every other way out depends on something working: the quit chord needs
    // input routing, and the control socket needs another terminal. This needs
    // only the event loop, so a run that comes up wrong still ends by itself
    // rather than holding the machine.
    // On a timerfd, for the same reason the frame clock is.
    //
    // With the shell running, GLib is the outer loop and calloop is dispatched
    // from inside it — and `glib_loop::prepare` passes -1, so GLib blocks until
    // one of the file descriptors it watches signals. calloop keeps its own
    // timers in a heap rather than as descriptors, so nothing about a calloop
    // timer expiring is visible to GLib: the deadline passes and the loop stays
    // asleep until something unrelated wakes it.
    //
    // That made `--exit-after` fail exactly where it is used. A busy compositor
    // exits roughly on time because client traffic wakes the loop anyway; an
    // idle one — a test, a CI run, a headless check — never does, and the flag
    // silently did nothing until an outer `timeout` killed the process. It cost
    // two runs on a spare VT, both of which left the display on a compositor
    // that should have stopped and had not.
    //
    // A timerfd is a descriptor like any other, so GLib wakes for it.
    if let Some(seconds) = flag(&args, "--exit-after").and_then(|v| v.parse::<u64>().ok()) {
        use smithay::reexports::calloop::generic::Generic;
        use smithay::reexports::calloop::{Interest, Mode, PostAction};
        use smithay::reexports::rustix::time::{
            timerfd_create, timerfd_settime, Itimerspec, TimerfdClockId, TimerfdFlags,
            TimerfdTimerFlags, Timespec,
        };

        tracing::info!("will exit after {seconds}s");
        let fd = timerfd_create(
            TimerfdClockId::Monotonic,
            TimerfdFlags::NONBLOCK | TimerfdFlags::CLOEXEC,
        )
        .map_err(|e| anyhow::anyhow!("creating the exit timer: {e}"))?;

        let spec = Itimerspec {
            it_interval: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: Timespec {
                tv_sec: seconds as _,
                tv_nsec: 0,
            },
        };
        timerfd_settime(&fd, TimerfdTimerFlags::empty(), &spec)
            .map_err(|e| anyhow::anyhow!("arming the exit timer: {e}"))?;

        event_loop
            .handle()
            .insert_source(
                Generic::new(fd, Interest::READ, Mode::Level),
                |_, fd, state: &mut ViewportState| {
                    // Drained, or a level-triggered source reports the same
                    // expiry for ever and the loop never sleeps again.
                    let mut buf = [0u8; 8];
                    let _ = smithay::reexports::rustix::io::read(&*fd, &mut buf[..]);
                    tracing::info!("the --exit-after deadline passed; stopping");
                    // `shutdown`, not `loop_signal.stop()`. With the web engine
                    // on, calloop is the inner loop and stopping it leaves GLib
                    // running — the deadline was reported and the process
                    // carried on regardless, which is the other half of why
                    // this flag never worked.
                    state.shutdown();
                    Ok(PostAction::Continue)
                },
            )
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
                // How fast the shell is painting, which the frame counter on
                // its own cannot say.
                state.report_shell_rate();
                // And whether the shell process is still there. Reaped here
                // rather than on SIGCHLD: this compositor installs no signal
                // handlers, and a desktop that has been gone for at most a
                // second is not a thing anyone can see.
                state.check_client_shell();
                // And the wallpaper terminal, on the same terms.
                state.check_background_terminal();
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
                        // And tell the sender, which the specification requires
                        // and this did not do: a notification closed by
                        // `CloseNotification` must be reported with reason 3.
                        // Without it an application that closes its own
                        // notification — a progress bar finishing, a chat
                        // client clearing a message it has shown — is never
                        // told it happened, and one that tracks its own
                        // notifications waits for an answer that never comes.
                        state
                            .notifications
                            .closed(id, crate::notification::CloseReason::ByRequest);
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

    // calloop owns the loop, with or without the web engine.
    //
    // It used to be GLib's, with calloop nested inside as a single source,
    // because WebKit needs a GMainContext pumped on the thread that owns the
    // web view. The web view is on its own thread now with a context of its
    // own, so nothing on this thread needs GLib at all — and being inside it
    // was expensive. GLib walks every source in its context on every turn of
    // the loop, the loop turns about twice per client message, and a client
    // committing thirteen thousand times a second therefore paid for twenty-six
    // thousand of those walks a second.
    //
    // The callback runs after each dispatch, which is calloop's version of the
    // "before it blocks again" hook the GSource's `prepare` was. Both things
    // that happened there still happen here, and for the reasons written up in
    // the commit that removed them:
    //
    // Drawing, because rendering is driven by vblank and vblank stops when
    // nothing is submitted — so on a still screen there has to be something
    // else to carry a commit to an output.
    //
    // Flushing, because a reply written into a client's outgoing buffer is not
    // sent until something flushes it, and the only other flush is at the end
    // of a render. A client that connects to an idle compositor is otherwise
    // answered and never hears the answer, which looks exactly like a program
    // that takes seconds to start and then appears the moment a key is pressed.
    event_loop.run(None, &mut state, |state| {
        // Timed only when the counters are on. See `FrameLog::loop_turns`.
        let timing = state
            .udev
            .as_ref()
            .and_then(|udev| udev.frame_log.as_ref())
            .is_some();
        let mark = || timing.then(std::time::Instant::now);
        let since = |at: Option<std::time::Instant>| {
            at.map(|at| at.elapsed().as_nanos() as u64).unwrap_or(0)
        };

        // CPU spent since this callback last returned, which is exactly one
        // turn of `EventLoop::dispatch`. Thread CPU rather than wall clock, so
        // the time asleep in `epoll_wait` is not counted as work.
        if timing {
            let now = crate::udev::thread_cpu_nanos();
            if let Some(log) = state.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
                if log.cpu_at_turn > 0 {
                    log.dispatch_cpu_nanos += now.saturating_sub(log.cpu_at_turn);
                }
            }
        }

        let at = mark();
        state.render_if_needed();
        let rendered = since(at);

        let at = mark();
        let _ = state.display_handle.flush_clients();
        let flushed = since(at);

        if let Some(log) = state.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
            log.loop_turns += 1;
            log.render_nanos += rendered;
            log.flush_nanos += flushed;
        }
        // Last thing, so the next turn's difference starts from here.
        if timing {
            let now = crate::udev::thread_cpu_nanos();
            if let Some(log) = state.udev.as_mut().and_then(|udev| udev.frame_log.as_mut()) {
                log.cpu_at_turn = now;
            }
        }
    })?;
    Ok(())
}

/// The value after `name` on the command line.
/// The value of `--name value`, or of `--name=value`.
///
/// Both forms, because both are what people type and the second used to be
/// accepted silently and ignored — `--url=/path` set nothing, started the
/// default shell, and said nothing about why.
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    if let Some(joined) = args
        .iter()
        .find_map(|a| a.strip_prefix(name)?.strip_prefix('='))
    {
        return Some(joined);
    }
    let at = args.iter().position(|a| a == name)?;
    args.get(at + 1).map(String::as_str)
}

struct Opt {
    flag: &'static str,
    /// What follows it, for the help text. Empty for a switch.
    value: &'static str,
    what: &'static str,
}

/// Every option the compositor understands.
///
/// One table for two jobs: what `--help` prints, and what an unrecognised
/// option is checked against. They were two lists once — a flag added to the
/// parser and to neither is a flag that works and is warned about.
const OPTIONS: &[Opt] = &[
    Opt {
        flag: "--drm",
        value: "",
        what: "take the screens and the seat: a session of its own, from a TTY",
    },
    Opt {
        flag: "--headless",
        value: "",
        what: "no renderer and no window: everything but drawing, for tests",
    },
    Opt {
        flag: "--width",
        value: "N",
        what: "the headless output's width (default 1920)",
    },
    Opt {
        flag: "--height",
        value: "N",
        what: "and its height (default 1080)",
    },
    Opt {
        flag: "--config",
        value: "PATH",
        what: "the config file, instead of the default one",
    },
    Opt {
        flag: "--layout",
        value: "NAME",
        what: "tiling or scrolling, over whatever the config says",
    },
    Opt {
        flag: "--renderer",
        value: "NAME",
        what: "vulkan or gles, over $VIEWPORT_RENDERER",
    },
    Opt {
        flag: "--shell-backend",
        value: "NAME",
        what: "which engine draws the desktop; see docs/shell-backends.md",
    },
    Opt {
        flag: "--url",
        value: "URL",
        what: "a page to run instead of the bundled desktop",
    },
    Opt {
        flag: "--url-span",
        value: "",
        what: "give that page every monitor, not just the first",
    },
    Opt {
        flag: "--watch-shell",
        value: "",
        what: "reload the shell when its files change",
    },
    Opt {
        flag: "--background-terminal",
        value: "[CMD]",
        what: "a terminal for a wallpaper, running CMD if one is given",
    },
    Opt {
        flag: "--socket",
        value: "PATH",
        what: "the control socket, instead of the one named after the display",
    },
    Opt {
        flag: "--exit-after",
        value: "SECS",
        what: "stop after this long, in case stopping is what is broken",
    },
];

const USAGE: &str = "\
usage: viewport [options]
       viewport msg [options] -t TYPE [--field value]...

A Wayland compositor whose desktop is a web page. With no options it nests
inside the session it was started from, which is what trying it costs a window
rather than the machine; --drm takes the screens instead.
";

fn print_help() {
    print!("{USAGE}");
    println!();
    println!("Options:");
    for option in OPTIONS {
        let flag = if option.value.is_empty() {
            option.flag.to_owned()
        } else {
            format!("{} {}", option.flag, option.value)
        };
        println!("  {flag:<28} {}", option.what);
    }
    let help = "-h, --help";
    println!("  {help:<28} this");
    println!();
    println!("Subcommands:");
    let msg = "msg";
    println!("  {msg:<28} drive a running compositor over its control socket;");
    let pad = "";
    println!("  {pad:<28} `viewport msg --help` lists every message");
    println!();
    println!("$VIEWPORT_LOG takes an env_logger filter — `VIEWPORT_LOG=debug` for");
    println!("everything the shell says. docs/configuration.md is the rest.");
}

/// Say so when an option is not one of ours.
///
/// Unknown arguments were dropped in silence, so a misremembered flag looked
/// exactly like a flag that had been honoured and done nothing.
fn warn_about_unknown_options(args: &[String]) {
    for arg in args.iter().skip(1).filter(|a| a.starts_with("--")) {
        let name = arg.split_once('=').map(|(n, _)| n).unwrap_or(arg);
        if !OPTIONS.iter().any(|option| option.flag == name) {
            tracing::warn!("unknown option {name}; it has been ignored");
        }
    }
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
    const VARIABLES: [&str; 3] = ["WAYLAND_DISPLAY", "XDG_CURRENT_DESKTOP", "XDG_SESSION_TYPE"];

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

    let mut children = Vec::new();
    for (program, arguments) in commands {
        match std::process::Command::new(program).args(&arguments).spawn() {
            Ok(child) => children.push(child),
            Err(e) => tracing::debug!("could not run {program}: {e}"),
        }
    }

    // One thread for both: they exit at once, a compositor that never waits
    // would leave two zombies for the life of the session, and the session
    // target has to be started after them rather than beside them. Bringing
    // the target up activates units, and a unit activated before the variables
    // land is one that starts without them — which for the portal backends is
    // the failure this whole function exists to avoid.
    std::thread::spawn(move || {
        for mut child in children {
            let _ = child.wait();
        }
        start_session_target();
    });
}

/// Put the user manager into a graphical session, which nothing else here will
/// do.
///
/// xdg-desktop-portal has guarded its unit with
///
///   Requisite=graphical-session.target
///
/// since 1.22, and a requisite that is inactive fails the job outright instead
/// of pulling it in. Nothing activates that target on its own: it sets
/// RefuseManualStart, so the compositor cannot start it directly, and the
/// display manager that would is not in the picture for a session launched
/// from a TTY. viewport-session.target is bound to it and carries no such
/// guard, so starting that one brings both up.
///
/// The symptom when this is missing is remote from the cause: the portal
/// frontend never starts, so `org.freedesktop.portal.Settings` is unanswered,
/// so every application reads no colour scheme and draws itself light. The
/// compositor's own Settings backend is on the bus and correct the whole time
/// — there is simply no frontend in front of it.
///
/// Failure is fine, and quiet on purpose: a session without systemd, or one
/// where the unit was never installed, wants none of this and works regardless.
fn start_session_target() {
    // --no-block because the target pulls in whatever the session declares
    // wants of it, and this thread has no reason to wait for any of it.
    match std::process::Command::new("systemctl")
        .args(["--user", "start", "--no-block", "viewport-session.target"])
        .spawn()
    {
        Ok(mut child) => {
            let _ = child.wait();
        }
        Err(e) => tracing::debug!("could not start viewport-session.target: {e}"),
    }
}
