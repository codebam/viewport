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
mod bluetooth;
mod capture;
mod clipboard;
// Not gated on the web engine: an output composite is worth capturing
// whatever is drawing into it.
mod color_management;
mod config;
mod cursor;
mod dbus;
mod dbus_util;
mod dump;
mod focus;
mod foreign_toplevel;
mod framing;
mod gamma;
mod handlers;
mod hdr;
mod headless;
mod icon;
mod idle;
mod inhibit;
mod input;
mod ipc;
mod keyboard_focus;
mod launcher;
mod libei;
mod lock;
mod mpris;
mod msg;
mod network;
mod notification;
mod output_management;
mod output_power;
mod pointer;
mod power;
mod recovery;
mod render;
mod rounded;
mod screencast;
mod screencopy;
mod screenshot;
mod session;
#[cfg(feature = "wpe")]
mod shell;
mod shell_backend;
mod shell_client;
mod shell_watch;
mod shortcuts;
mod sound;
mod state;
mod status;
mod tearing;
mod tray;
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
    // A valued option followed by another option took that option as its
    // value: `--drm --width --height 800` parsed `--width`'s value as
    // "--height", failed to parse it as a number, and quietly started at the
    // default — with the 800 the user asked for landing nowhere. The same
    // refusal the `viewport msg` parser makes (`msg.rs`): a value that looks
    // like another flag is no value at all.
    for option in OPTIONS {
        // Switches take nothing, and a value in brackets may be left off —
        // `--background-terminal --drm` is the bare form, not a mistake.
        if option.value.is_empty() || option.value.starts_with('[') {
            continue;
        }
        let Some(at) = args.iter().position(|argument| argument == option.flag) else {
            continue;
        };
        match args.get(at + 1) {
            None => anyhow::bail!("{0} needs a value ({1})", option.flag, option.value),
            Some(next) if next.starts_with("--") => anyhow::bail!(
                "{0} needs a value ({1}); {2} looks like another option",
                option.flag,
                option.value,
                next
            ),
            Some(_) => {}
        }
    }
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

    // The scanout bit depth, resolved here and carried in the environment
    // because the DRM device picks its formats while it is being opened —
    // before there is a state to read a setting out of. `--renderer` takes the
    // same route for the same reason.
    //
    // Command line over environment over file: the more deliberate of the
    // three wins, and the file is the standing preference. Validated here
    // rather than where it is read, because a typo should be a message at
    // startup and not a warning buried in the log of a session that is already
    // running on the wrong depth.
    {
        let asked = flag(&args, "--pixel-format")
            .map(str::to_owned)
            .or_else(|| std::env::var("VIEWPORT_PIXEL_FORMAT").ok())
            .or_else(|| config.pixel_format.clone());
        if let Some(asked) = asked {
            config::parse_pixel_format(&asked)
                .map_err(|e| anyhow::anyhow!("{e}; --pixel-format, $VIEWPORT_PIXEL_FORMAT or the config file's \"pixel_format\""))?;
            // SAFETY: single-threaded still — no backend is up and nothing has
            // been spawned, which is the same window `--renderer` uses.
            unsafe { std::env::set_var("VIEWPORT_PIXEL_FORMAT", &asked) };
        }
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
    state.config_file_path = config_path.clone();
    if shell_watch::wanted(&args) {
        state.watch_shell_assets();
        state.watch_config_file();
    } else if shell_watch::config_wanted(&args) {
        state.watch_config_file();
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

    // A picture for a wallpaper, from the command line.
    //
    // After the config, so the flag wins — the rule every other flag here
    // follows. Fatal rather than a warning, unlike the same key in the config
    // file: a path typed on the command line was typed just now, and starting
    // anyway would answer it with a desktop that looks exactly like one that
    // ignores the flag. The file gets the gentler treatment because it is read
    // again on every reload of a session already running.
    if let Some(wallpaper) = flag(&args, "--wallpaper") {
        let resolved = config::wallpaper_value(wallpaper, "--wallpaper")?;
        tracing::info!("wallpaper from the command line: {resolved}");
        state.config.wallpaper = Some(resolved);
    }
    if let Some(mode) = flag(&args, "--wallpaper-mode") {
        let mode = config::parse_wallpaper_mode(mode).map_err(|e| {
            anyhow::anyhow!("{e}; --wallpaper-mode or the config file's \"wallpaper_mode\"")
        })?;
        state.config.wallpaper_mode = Some(mode);
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

    // Said out loud so a log can be read without having to trust that an
    // environment variable survived whatever launched the compositor. A
    // diagnostic that is silent when it is off and silent when it is on is
    // not a diagnostic.
    tracing::info!(
        "pointer capture narration is {}",
        if pointer::debug() {
            "on"
        } else {
            "off (VIEWPORT_POINTER_DEBUG=1 turns it on)"
        }
    );

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
    // DISPLAY. It arrives asynchronously; the number is recorded on the state
    // when it does, and every child this compositor spawns from then on is
    // told it outright rather than finding it in an environment that is never
    // written again once the workers are up.
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
                // And whether one that died earlier has waited out the backoff
                // its run of crashes earned.
                state.start_due_shells();
                // And the wallpaper terminal, on the same terms.
                state.check_background_terminal();
                // And the things whose owner is gone but which nothing else
                // notices: screencopy requests for outputs that stopped being
                // drawn, screenshot files the portal has long since handed
                // out. Both hold memory or disk until someone lets go.
                state.reap_pending_copies();
                state.reap_screenshot_temps();
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
                        // Kept before it is drawn. The popup is the part that
                        // goes away; this is the part that is still there
                        // afterwards, and a shell that never draws the popup
                        // — one still starting, one on a blanked screen —
                        // has not cost the record of it.
                        if state.notification_history.record(&notification) {
                            state.publish_notification_history();
                        }
                        state.notify(&viewport_ipc::Event::NotificationAdd(*notification));
                    }
                    crate::notification::Message::Close(id) => {
                        // Withdrawn by the sender rather than seen and
                        // dismissed by the user: a progress bar that finished,
                        // a chat message read on the phone instead. Keeping it
                        // in the centre would be keeping a message its own
                        // application says is no longer true.
                        if state.notification_history.forget(id) {
                            state.publish_notification_history();
                        }
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

    // The system tray, on the same shape: a thread on the bus, a channel back,
    // and the shell drawing what arrives. See src/tray.rs.
    {
        let (sender, source) = smithay::reexports::calloop::channel::channel();
        event_loop
            .handle()
            .insert_source(source, |event, _, state| {
                use smithay::reexports::calloop::channel::Event;
                let Event::Msg(message) = event else {
                    return;
                };
                let items = match message {
                    crate::tray::Message::Items(items) => items,
                    crate::tray::Message::Menu { id, x, y, items } => {
                        state.notify(&viewport_ipc::Event::TrayMenu { id, x, y, items });
                        return;
                    }
                };
                // One line per change, not per sample: this fires when an
                // application registers, exits or says its icon changed. What
                // it answers is the question a tray raises first — whether the
                // compositor sees the item at all, or whether the icon is
                // missing further along.
                tracing::info!(
                    "tray: {} item(s){}",
                    items.len(),
                    items
                        .iter()
                        .map(|item| format!(" {}", item.title))
                        .collect::<String>()
                );
                state.notify(&viewport_ipc::Event::TrayUpdate { items });
            })
            .map_err(|e| anyhow::anyhow!("inserting the tray source: {e}"))?;

        // Now, rather than when the configuration was read: the configuration
        // is applied before this loop has anywhere to send a tray, so what it
        // asked for was remembered and is acted on here.
        let enabled = state.tray_enabled;
        state.tray.attach(sender, enabled);
    }

    // What is playing, on the same shape again — and idle until a bar widget
    // asks for it, which `apply_config` decides.
    {
        let (sender, source) = smithay::reexports::calloop::channel::channel();
        event_loop
            .handle()
            .insert_source(source, |event, _, state| {
                use smithay::reexports::calloop::channel::Event;
                let Event::Msg(crate::mpris::Message::Player(player)) = event else {
                    return;
                };
                state.notify(&viewport_ipc::Event::MprisUpdate { player });
            })
            .map_err(|e| anyhow::anyhow!("inserting the media source: {e}"))?;
        state.mpris.attach(sender);
    }

    // Battery, lid and power profiles. Same shape as MPRIS: idle until
    // `apply_config` decides a widget or a lid policy wants it.
    {
        let (sender, source) = smithay::reexports::calloop::channel::channel();
        event_loop
            .handle()
            .insert_source(source, |event, _, state| {
                use smithay::reexports::calloop::channel::Event;
                let Event::Msg(crate::power::Message::Snapshot(snapshot)) = event else {
                    return;
                };
                state.handle_power(snapshot);
            })
            .map_err(|e| anyhow::anyhow!("inserting the power source: {e}"))?;
        state.power.attach(sender);
    }

    // The two radios, on the same shape again: a worker thread each, started
    // by the first request from a picker rather than by the configuration.
    // Which one is wanted is a question only the shell can answer, because the
    // answer is whether somebody has a picker open.
    {
        let (sender, source) = smithay::reexports::calloop::channel::channel();
        event_loop
            .handle()
            .insert_source(source, |event, _, state| {
                use smithay::reexports::calloop::channel::Event;
                let Event::Msg(crate::network::Message::Snapshot(snapshot)) = event else {
                    return;
                };
                state.notify(&viewport_ipc::Event::NetworkUpdate(snapshot));
            })
            .map_err(|e| anyhow::anyhow!("inserting the network source: {e}"))?;
        state.network.attach(sender);
    }
    {
        let (sender, source) = smithay::reexports::calloop::channel::channel();
        event_loop
            .handle()
            .insert_source(source, |event, _, state| {
                use smithay::reexports::calloop::channel::Event;
                let Event::Msg(crate::bluetooth::Message::Snapshot(snapshot)) = event else {
                    return;
                };
                state.notify(&viewport_ipc::Event::BluetoothUpdate(snapshot));
            })
            .map_err(|e| anyhow::anyhow!("inserting the Bluetooth source: {e}"))?;
        state.bluetooth.attach(sender);
    }

    // The clipboard history. The reading happens on a thread — the other end
    // of a selection is a pipe to another process — and what comes back lands
    // here.
    {
        let (sender, source) = smithay::reexports::calloop::channel::channel();
        event_loop
            .handle()
            .insert_source(source, |event, _, state| {
                use smithay::reexports::calloop::channel::Event;
                let Event::Msg(crate::clipboard::Message::Copied(text)) = event else {
                    return;
                };
                if state.clipboard.record(text) {
                    state.notify_clipboard();
                }
            })
            .map_err(|e| anyhow::anyhow!("inserting the clipboard source: {e}"))?;
        state.clipboard.attach(sender);
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

        let (screenshot_sender, screenshot_source) =
            smithay::reexports::calloop::channel::channel();
        event_loop
            .handle()
            .insert_source(screenshot_source, |event, _, state| {
                use smithay::reexports::calloop::channel::Event;
                let Event::Msg(message) = event else {
                    return;
                };
                state.handle_screenshot(message);
            })
            .map_err(|e| anyhow::anyhow!("inserting the screenshot source: {e}"))?;

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

        // Global shortcuts, on the same connection and answered by the same
        // chooser. It needs the screencast object's session table for one
        // question only — whether a call came from the portal frontend — and
        // its own channel for everything else, because what it asks the
        // compositor for is a key rather than a picture.
        let (shortcuts_sender, shortcuts_source) = smithay::reexports::calloop::channel::channel();
        event_loop
            .handle()
            .insert_source(shortcuts_source, |event, _, state| {
                use smithay::reexports::calloop::channel::Event;
                let Event::Msg(message) = event else {
                    return;
                };
                state.handle_shortcuts(message);
            })
            .map_err(|e| anyhow::anyhow!("inserting the shortcuts source: {e}"))?;
        let shortcuts =
            crate::shortcuts::GlobalShortcuts::new(shortcuts_sender, screencast.sessions());
        let screenshot = crate::screenshot::Screenshot::new(screenshot_sender);

        // The inhibit backend goes up with them, on the same connection and
        // for the same reason: one bus name, so whichever interface was built
        // second would otherwise be missing from the bus entirely.
        let inhibit = crate::inhibit::PortalInhibit::new(state.bus_inhibitors.clone());

        // Not fatal: a real desktop portal already holding the name knows more
        // about the session than this does, and applications keep the defaults
        // they had a moment ago.
        if let Err(e) = state
            .appearance
            .start(settings, screencast, screenshot, inhibit, shortcuts)
        {
            tracing::warn!("the portals are unavailable: {e}");
        }
        if let Some(connection) = state.appearance.connection() {
            // So a request abandoned by a frontend that crashed can be taken
            // off the bus, not only out of the table. See
            // `inhibit::watch_owners`.
            state
                .bus_inhibitors
                .set_portal_connection(connection.clone());
            // And how a chord that fired reaches the application waiting for
            // it: the signal goes out on the connection the interface is
            // served from, and the compositor is not the thread that built it.
            state.shortcut_signals.set_connection(connection);
        }
    }

    // And the screensaver interface, which is the one a browser actually uses
    // to keep the screen awake. A connection of its own, because it is a bus
    // name of its own — see `crate::inhibit`.
    {
        let (sender, source) = smithay::reexports::calloop::channel::channel();
        event_loop
            .handle()
            .insert_source(source, |event, _, state| {
                use smithay::reexports::calloop::channel::Event;
                let Event::Msg(crate::inhibit::Message::Activity) = event else {
                    return;
                };
                // The same path a keypress takes, including bringing blanked
                // screens back: a program saying somebody is there means the
                // same thing as somebody being there.
                if state.idle.activity(crate::idle::Activity::Deliberate) {
                    state.set_outputs_enabled(true);
                }
                let seat = state.seat.clone();
                state.idle_notifier_state.notify_activity(&seat);
            })
            .map_err(|e| anyhow::anyhow!("inserting the inhibit source: {e}"))?;

        // Not fatal, on the same terms as the rest: a session with no bus, or
        // one where a screensaver daemon already holds the name, has the idle
        // policy it had a moment ago.
        match crate::inhibit::start(state.bus_inhibitors.clone(), sender) {
            Ok(connection) => state.screensaver = Some(connection),
            Err(e) => tracing::warn!("the screensaver interface is unavailable: {e}"),
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

        // The half of a sample that has to be waited for — `statvfs` on every
        // configured mount, and `wpctl` for the volume and the microphone —
        // happens on a thread of its own and arrives here, exactly as a
        // notification does. Waiting for it on this thread stalled the
        // compositor twice a second on any bar with a volume widget, and for
        // good on any machine with a mount whose server had gone.
        let (sender, source) = smithay::reexports::calloop::channel::channel();
        event_loop
            .handle()
            .insert_source(source, |event, _, state| {
                use smithay::reexports::calloop::channel::Event;
                let Event::Msg(slow) = event else {
                    return;
                };
                // A volume that changed is told to the shell now rather than on
                // the next tick: a scroll on the bar is answered while the
                // finger is still moving. Everything else waits for the tick.
                if state.status.absorb(slow) {
                    state.status_tick();
                }
            })
            .map_err(|e| anyhow::anyhow!("inserting the status source: {e}"))?;

        // Not fatal: a compositor that cannot spawn the thread samples the slow
        // half in line, which is what it did before there was a thread.
        if let Err(e) = state.status.start(sender) {
            tracing::warn!("the status worker could not start: {e}");
        }
    }

    // The launcher's scan, on the status sampler's pattern. Every keystroke
    // in the shell's picker asks for the applications list, and answering one
    // is read_dir over every applications directory, read_to_string over
    // every `.desktop` file that survives it, and an icon found in the theme
    // and base64-encoded out of its file per row shown — a hundred milliseconds
    // of that per keystroke, right here between two frames, before the scan
    // moved to a thread of its own. The query is posted and the answer comes
    // back through a calloop channel like anything else the loop hears.
    {
        let (sender, source) = smithay::reexports::calloop::channel::channel();
        event_loop
            .handle()
            .insert_source(source, |event, _, state| {
                use smithay::reexports::calloop::channel::Event;
                let Event::Msg(answer) = event else {
                    return;
                };
                state.launcher_apply(answer);
            })
            .map_err(|e| anyhow::anyhow!("inserting the launcher source: {e}"))?;

        // Not fatal, on the same terms as the status worker: a compositor that
        // cannot spawn the thread answers the picker in line, which is what it
        // did before there was a thread.
        if let Err(e) = state.launcher_scan.start(sender) {
            tracing::warn!("the launcher worker could not start: {e}");
        }
    }

    // The lock screen's password check, on a thread of its own.
    //
    // A PAM conversation blocks — on a file, on a slow hash, on the network if
    // the stack reaches for LDAP or Kerberos, and on `pam_fail_delay`'s two
    // seconds after a wrong password. Every one of those on this thread is the
    // whole desk frozen while somebody types at the lock screen, so the
    // attempt goes to a worker and the verdict comes back here like anything
    // else the loop hears. See `crate::lock`.
    {
        let (sender, source) = smithay::reexports::calloop::channel::channel();
        event_loop
            .handle()
            .insert_source(source, |event, _, state| {
                use smithay::reexports::calloop::channel::Event;
                let Event::Msg(verdict) = event else {
                    return;
                };
                state.handle_lock_verdict(verdict);
            })
            .map_err(|e| anyhow::anyhow!("inserting the authentication source: {e}"))?;

        // Not fatal, and the failure is in the safe direction: with no worker
        // the lock screen is drawn, the session locks, and no password is ever
        // accepted. `Authenticator::online` is false, the page is told so at
        // the lock, and `lock_with_shell` says it in the log — because a
        // password box that cannot open is the one failure somebody will sit
        // in front of for a long time before suspecting the compositor.
        if let Err(e) = state.authenticator.start(sender) {
            tracing::error!(
                "the authentication worker could not start: {e}. The built-in \
                 lock screen will refuse every password; set idle.lock_command \
                 to lock this machine with a locker of its own."
            );
        }
    }

    // Whatever the config file asked to be run, once everything it needs is
    // in the environment: WAYLAND_DISPLAY (inherited), DISPLAY (told to it
    // outright — Xwayland may not even be up yet), and the outputs.
    if let Some(command) = state.startup.clone() {
        tracing::info!("startup: {command}");
        input::spawn_with_env(&command, &state.child_display_env());
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

        // Before the frame: a buffer destroyed on this turn's dispatch is one
        // whose image the renderer is still holding, and the renderer is about
        // to be moved out to draw with.
        state.forget_dead_buffers();
        // And the capture buffers, once there is nothing left to capture for.
        state.release_capture_scratch();

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

    // The loop has stopped; the display and everything else is still up. That
    // is the one moment a shell can be stopped in the order it was written for
    // — socket first, engine next, display last. Dropping out of here instead
    // took the display away under a running engine, and the shell died of it.
    // See `ViewportState::stop_client_shells`.
    state.stop_client_shells();
    Ok(())
}

/// The value after `name` on the command line.
/// The value of `--name value`, or of `--name=value`.
///
/// Both forms, because both are what people type and the second used to be
/// accepted silently and ignored — `--url=/path` set nothing, started the
/// default shell, and said nothing about why.
///
/// A following argument that is itself a flag is not a value here; that case
/// is refused outright before any of this runs, so what `--width --height`
/// gets is an error and not a width of "--height".
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
        what: "tiling, scrolling, solar, matrix or canvas, over the config",
    },
    Opt {
        flag: "--renderer",
        value: "NAME",
        what: "vulkan or gles, over $VIEWPORT_RENDERER",
    },
    Opt {
        flag: "--pixel-format",
        value: "N",
        what: "scanout bits per channel: 8, 10 or auto, over $VIEWPORT_PIXEL_FORMAT",
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
        flag: "--watch-config",
        value: "",
        what: "reload the configuration file when it changes",
    },
    Opt {
        flag: "--background-terminal",
        value: "[CMD]",
        what: "a terminal for a wallpaper, running CMD if one is given",
    },
    Opt {
        flag: "--wallpaper",
        value: "PATH",
        what: "a picture for the desktop background, over the config file",
    },
    Opt {
        flag: "--wallpaper-mode",
        value: "NAME",
        what: "how it is fitted: fill, fit, stretch, center or tile",
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
    // The cursor pair among them: a client started by systemd or activated over
    // D-Bus does not inherit this process's environment, and one that cannot
    // read XCURSOR_SIZE picks its own default — which is a pointer that changes
    // size depending on how the application happened to be launched.
    const VARIABLES: [&str; 5] = [
        "WAYLAND_DISPLAY",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_TYPE",
        "XCURSOR_THEME",
        "XCURSOR_SIZE",
    ];

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
        restart_stale_portal();
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
    // Waited for, rather than --no-block as this once was: the portal frontend
    // is restarted immediately after, and that unit carries
    // Requisite=graphical-session.target. A restart job queued while this
    // target is still coming up is a job that fails outright.
    match std::process::Command::new("systemctl")
        .args(["--user", "start", "viewport-session.target"])
        .spawn()
    {
        Ok(mut child) => {
            let _ = child.wait();
        }
        Err(e) => tracing::debug!("could not start viewport-session.target: {e}"),
    }
}

/// Restart a portal frontend that was started by somebody else's session.
///
/// xdg-desktop-portal reads its configuration once, at startup, keyed on the
/// XDG_CURRENT_DESKTOP the user manager held at that moment, and it builds one
/// proxy per interface then and there. Both of those go stale the moment a
/// second compositor takes over a user manager that never went down:
///
///   * The backend list is the other desktop's. A frontend started under sway
///     read sway-portals.conf, so ScreenCast is xdg-desktop-portal-wlr — which
///     captures through wlr-screencopy against a compositor that is gone, and
///     answers "no output found".
///   * The properties it publishes are whatever the proxy read when it was
///     built. `AvailableSourceTypes` is 0 when this compositor was not yet on
///     the bus, and nothing refreshes it afterwards.
///
/// That second one is why this shows up in one browser and not the other.
/// Firefox calls SelectSources regardless and gets whatever the backend does;
/// Chromium reads AvailableSourceTypes first, sees no source type it can use,
/// decides the portal cannot share a screen, and falls back to its own
/// getUserMedia picker — the tab list, with no screens or windows in it. No
/// error is logged on either side: from the portal's point of view nobody
/// asked.
///
/// try-restart rather than restart: a session that boots straight into this
/// compositor has no frontend running yet, and starting one here would only
/// race the D-Bus activation that is about to happen anyway. try-restart is a
/// no-op on an inactive unit and a restart on a live one, which is exactly the
/// distinction that matters. The backends go with it, for the same reason: one
/// left over from another session holds a connection to a compositor that
/// exited.
fn restart_stale_portal() {
    match std::process::Command::new("systemctl")
        .args([
            "--user",
            "try-restart",
            "xdg-desktop-portal.service",
            "xdg-desktop-portal-wlr.service",
            "xdg-desktop-portal-gtk.service",
        ])
        .spawn()
    {
        Ok(mut child) => {
            let _ = child.wait();
        }
        Err(e) => tracing::debug!("could not restart the portal frontend: {e}"),
    }
}
