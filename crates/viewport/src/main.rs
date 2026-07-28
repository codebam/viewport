// SPDX-License-Identifier: GPL-3.0-or-later
//
// Viewport, in Rust. Ports src/main.c.
//
// See docs/RUST-REWRITE.md for what is deliberately absent. The short version:
// there is no web engine yet, so the shell is a flat backdrop and windows are
// placed by whatever speaks to the control socket.

mod apply;
mod binding;
#[cfg(feature = "wpe")]
mod dump;
mod color_management;
mod cursor;
mod framing;
#[cfg(feature = "wpe")]
mod glib_loop;
mod handlers;
mod headless;
mod input;
mod ipc;
mod session;
#[cfg(feature = "wpe")]
mod shell;
mod state;
mod udev;
mod views;
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

    let mut event_loop: EventLoop<ViewportState> = EventLoop::try_new()?;
    let display: Display<ViewportState> = Display::new()?;

    let mut state = ViewportState::new(&mut event_loop, display, socket_path)?;
    if drm {
        udev::init(&mut event_loop, &mut state)?;
    } else if headless {
        let width = flag(&args, "--width").and_then(|v| v.parse().ok()).unwrap_or(1920);
        let height = flag(&args, "--height").and_then(|v| v.parse().ok()).unwrap_or(1080);
        headless::init(&mut event_loop, &mut state, width, height)?;
    } else {
        winit::init(&mut event_loop, &mut state)?;
    }

    // Child processes should reach this compositor rather than the host one.
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };

    tracing::info!(
        "viewport {} on {} (smithay rewrite)",
        env!("CARGO_PKG_VERSION"),
        state.socket_name.to_string_lossy()
    );

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
