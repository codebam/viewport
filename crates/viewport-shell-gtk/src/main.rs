// SPDX-License-Identifier: MIT
//
// The shell, out of process, rendered by WebKitGTK.
//
// The in-process backend (`--features wpe`) embeds WPE WebKit: the compositor
// owns the engine, drives its main loop, and receives a DMA-BUF per frame. It
// works, and it costs a four-hour WebKit build that no binary cache has,
// because `wpewebkit` is packaged by nobody.
//
// This is the same shell against an engine that *is* packaged. WebKitGTK 6.0
// is prebuilt in nixpkgs — the same WebKit 2.52.5 the flake otherwise compiles
// as WPE — so the trade is: give up owning the engine, and get the engine for
// free.
//
// What that means concretely, and why it is a smaller change than it sounds:
//
// * **Pixels.** This is an ordinary Wayland client. It paints into a DMA-BUF
//   and attaches it to a surface, and the compositor imports that buffer the
//   way it imports every other client's. The zero-copy path is the one that
//   already existed; nothing about it is shell-specific any more.
//
// * **Input.** Also ordinary. The WPE backend has to translate every pointer
//   and key event into an engine call (`crates/viewport/src/shell.rs`,
//   `Command::PointerMotion` and friends); here the compositor sends
//   `wl_pointer` and `wl_keyboard` events to a surface, GTK turns them into
//   GDK events, and WebKit sees exactly what it would see in a browser.
//
// * **Frame pacing.** Also ordinary: `wl_surface::frame`. The invitation the
//   compositor already sends every other client is what paces the shell.
//
// * **The bridge.** WebKitGTK has the same user-content API WPE does, so
//   `window.webkit.messageHandlers.viewport` exists here for real rather than
//   being shimmed in, and `data/shell/*.js` needs no edit at all. The far side
//   is the compositor's control socket, which already speaks exactly this
//   protocol to `viewportctl` and the tests.
//
// The one thing that is genuinely different: the shell can now die without
// taking the compositor with it, and the compositor can restart it without
// restarting itself.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;

use anyhow::{anyhow, Context as _, Result};
use gtk4 as gtk;
// One prelude, not two: webkit6's re-exports the GTK traits this uses, and
// importing both is an unused import rather than a missing one.
use webkit6::prelude::*;

/// The `app_id` this window carries.
///
/// Not how the compositor identifies the shell — that is done by handing this
/// process a Wayland connection it created for the purpose, which no other
/// client can claim. This is for everything else: a taskbar, a screen reader,
/// a log.
const APP_ID: &str = "dev.viewport.shell";

/// How many web-process crashes before this is a fault rather than a mishap.
const WEB_PROCESS_CRASH_LIMIT: u32 = 3;

/// The exit status that asks the compositor to start this again without
/// WebKit's DMA-BUF renderer. Read by `crate::shell_client` on the compositor
/// side; the number is arbitrary and only has to be one nothing else uses.
const RETRY_WITHOUT_DMABUF: i32 = 87;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let options = Options::parse(&args)?;

    // GTK must not see our arguments. It parses what it recognises and
    // complains about the rest, and `--url` is not one of its.
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let failure: Rc<std::cell::RefCell<Option<anyhow::Error>>> = Rc::new(Default::default());

    {
        let options = options.clone();
        let failure = failure.clone();
        app.connect_activate(move |app| {
            if let Err(e) = activate(app, &options) {
                *failure.borrow_mut() = Some(e);
                app.quit();
            }
        });
    }

    let code = app.run_with_args::<&str>(&[]);

    if let Some(e) = failure.borrow_mut().take() {
        return Err(e);
    }
    if code != gtk::glib::ExitCode::SUCCESS {
        return Err(anyhow!("the shell exited with {code:?}"));
    }
    Ok(())
}

/// Everything the shell needs to be told, and where each of them comes from.
#[derive(Clone, Debug)]
struct Options {
    /// The page to load.
    url: String,
    /// The compositor's control socket.
    socket: PathBuf,
    /// Whether to allow the web inspector. For a shell being edited live.
    inspector: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let flag = |name: &str| -> Option<String> {
            let mut it = args.iter();
            while let Some(arg) = it.next() {
                if let Some(rest) = arg.strip_prefix(&format!("{name}=")) {
                    return Some(rest.to_owned());
                }
                if arg == name {
                    return it.next().cloned();
                }
            }
            None
        };

        let url = flag("--url")
            .or_else(|| std::env::var("VIEWPORT_SHELL_URL").ok())
            .ok_or_else(|| {
                anyhow!(
                    "no page to load: pass --url or set VIEWPORT_SHELL_URL. \
                     The compositor sets it when it starts the shell itself"
                )
            })?;

        // The compositor passes the path outright, because a shell started
        // with `WAYLAND_SOCKET` has no `WAYLAND_DISPLAY` to derive it from.
        // The derivation is still here for running this by hand against a
        // compositor that is already up, which is the whole dev loop for the
        // shell's own JavaScript.
        let socket = match flag("--socket").or_else(|| std::env::var("VIEWPORT_IPC_SOCKET").ok()) {
            Some(path) => PathBuf::from(path),
            None => {
                let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
                let display = std::env::var("WAYLAND_DISPLAY").map_err(|_| {
                    anyhow!(
                        "no control socket: pass --socket or set VIEWPORT_IPC_SOCKET, \
                         or run under a compositor so WAYLAND_DISPLAY names one"
                    )
                })?;
                PathBuf::from(format!("{dir}/viewport-{display}.sock"))
            }
        };

        Ok(Self {
            url,
            socket,
            inspector: flag("--inspector").is_some()
                || std::env::var("VIEWPORT_SHELL_INSPECTOR").is_ok(),
        })
    }
}

fn activate(app: &gtk::Application, options: &Options) -> Result<()> {
    let manager = webkit6::UserContentManager::new();
    // This is the outbound half of the bridge, and it is the real thing rather
    // than the shim `viewport_web::BRIDGE_SHIM` installs for an engine without
    // it: registering the handler under the name `viewport` is what makes
    // `window.webkit.messageHandlers.viewport.postMessage` exist in the page.
    // `data/shell/state.js:13` looks it up by that exact path.
    manager.register_script_message_handler("viewport", None);

    let view = webkit6::WebView::builder()
        .user_content_manager(&manager)
        .build();

    // The shell paints the whole desktop, including the wallpaper, so there is
    // nothing behind it that should ever show through. Left at the default,
    // the page's own background is composited over WebKit's white, and a shell
    // that has not finished loading flashes white across every monitor.
    view.set_background_color(&gtk::gdk::RGBA::BLACK);

    if options.inspector {
        if let Some(settings) = webkit6::prelude::WebViewExt::settings(&view) {
            settings.set_enable_developer_extras(true);
        }
    }

    // The two ways a shell comes up blank, each of which is silent by default
    // and indistinguishable from the other in a screenshot: the page did not
    // load, or it loaded and the process rendering it died. The WPE backend
    // has a `CrashSink` for the second of these for exactly this reason.
    view.connect_load_failed(|_, _, uri, error| {
        tracing::error!("the shell page failed to load from {uri}: {error}");
        // False: let WebKit show its own error page. There is nothing better
        // to put there, and a blank one says even less than its does.
        false
    });
    // WebKit's web process can die without taking this one with it, and when
    // it does the window keeps showing the last frame it painted — a desktop
    // that is a photograph. So: relaunch it by reloading, which is what the
    // WPE backend's `CrashSink` does for the same event.
    //
    // And if it keeps happening, stop: a run of crashes in seconds is not a
    // process that was unlucky. Exiting with `RETRY_WITHOUT_DMABUF` asks the
    // compositor to start the shell again with WebKit's DMA-BUF renderer
    // turned off, because that is the path that has actually been seen to
    // fail — the page renders through shared memory inside WebKit after that,
    // one copy more, and the window's own buffer is still a DMA-BUF, so the
    // handoff to the compositor stays zero-copy either way.
    {
        let crashes = std::cell::Cell::new(0u32);
        let degraded = std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_some();
        view.connect_web_process_terminated(move |view, reason| {
            let count = crashes.get() + 1;
            crashes.set(count);
            tracing::error!("the web process died ({reason:?}), {count} time(s)");
            if count < WEB_PROCESS_CRASH_LIMIT {
                view.reload();
                return;
            }
            if degraded {
                tracing::error!(
                    "it has died {count} times with WebKit's DMA-BUF renderer already off; \
                     there is nothing further to try from here"
                );
                view.reload();
                return;
            }
            tracing::error!(
                "it has died {count} times; asking to be started again with WebKit's \
                 DMA-BUF renderer off"
            );
            std::process::exit(RETRY_WITHOUT_DMABUF);
        });
    }

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("viewport shell")
        // The compositor gives this window the whole output layout and draws
        // it under everything. A decoration would be a titlebar on the
        // desktop itself.
        .decorated(false)
        .child(&view)
        .build();

    let socket = UnixStream::connect(&options.socket)
        .with_context(|| format!("connecting to {}", options.socket.display()))?;

    bridge(&view, &manager, socket, app)?;

    view.load_uri(&options.url);
    window.present();
    Ok(())
}

/// Wire the page to the compositor, in both directions.
///
/// Neither direction runs on the GTK thread by itself. A blocking read there
/// would stop the shell painting, and a blocking write there would stop it as
/// well the moment the compositor was slow to drain the socket — which is
/// exactly when the desktop most needs to keep drawing. So each direction gets
/// a thread, and the GTK thread only ever touches a channel.
fn bridge(
    view: &webkit6::WebView,
    manager: &webkit6::UserContentManager,
    socket: UnixStream,
    app: &gtk::Application,
) -> Result<()> {
    let reader = socket.try_clone().context("duplicating the socket")?;

    // Page to compositor.
    let (out_tx, out_rx) = mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("ipc-write".into())
        .spawn(move || {
            let mut socket = socket;
            while let Ok(mut line) = out_rx.recv() {
                line.push('\n');
                if let Err(e) = socket.write_all(line.as_bytes()) {
                    tracing::error!("writing to the compositor: {e}");
                    return;
                }
            }
        })
        .context("starting the socket writer")?;

    manager.connect_script_message_received(Some("viewport"), move |_, value| {
        // The compositor accepts either a JSON string or a live object, so
        // page authors can call postMessage({...}) without stringifying by
        // hand (`src/web.c:63`). Preserve that.
        let json = if value.is_string() {
            value.to_str().to_string()
        } else {
            match value.to_json(0) {
                Some(json) => json.to_string(),
                None => {
                    tracing::warn!("the page posted something that is not JSON");
                    return;
                }
            }
        };
        if out_tx.send(json).is_err() {
            tracing::error!("the compositor is gone; the message was dropped");
        }
    });

    // Compositor to page.
    let (in_tx, in_rx) = async_channel::unbounded::<Line>();
    std::thread::Builder::new()
        .name("ipc-read".into())
        .spawn(move || {
            for line in BufReader::new(reader).lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(e) => {
                        tracing::error!("reading from the compositor: {e}");
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                if in_tx.send_blocking(Line::Event(line)).is_err() {
                    return;
                }
            }
            let _ = in_tx.send_blocking(Line::Closed);
        })
        .context("starting the socket reader")?;

    // Events that arrived before the page could receive them.
    //
    // The compositor starts broadcasting the moment it accepts the connection,
    // and the connection is made before the page has finished loading —
    // deliberately, so nothing that happens during the load is missed. A
    // script evaluated against a document that does not exist yet is dropped
    // on the floor, so they wait here instead.
    let queue: Rc<std::cell::RefCell<Vec<String>>> = Rc::new(Default::default());
    let loaded = Rc::new(std::cell::Cell::new(false));

    {
        let queue = queue.clone();
        let loaded = loaded.clone();
        view.connect_load_changed(move |view, event| match event {
            webkit6::LoadEvent::Committed => {
                // Committed, not Finished: the document and its scripts
                // exist here, and waiting for every subresource would hold
                // the desktop's state back behind a slow image.
                loaded.set(true);
                for json in queue.borrow_mut().drain(..) {
                    post(view, &json);
                }
            }
            webkit6::LoadEvent::Started => {
                // A reload throws the page away, so anything queued for
                // the old one is stale and anything sent to the new one
                // has to wait for it.
                loaded.set(false);
            }
            _ => {}
        });
    }

    {
        let view = view.clone();
        let app = app.clone();
        gtk::glib::spawn_future_local(async move {
            while let Ok(line) = in_rx.recv().await {
                match line {
                    Line::Event(json) => {
                        // The one message meant for the shell process rather
                        // than the page. The compositor's reload binding used
                        // to be a call into the engine it owned; out of
                        // process it has to travel like everything else.
                        if is_reload(&json) {
                            tracing::info!("reloading the shell");
                            view.reload_bypass_cache();
                            continue;
                        }
                        if loaded.get() {
                            post(&view, &json);
                        } else {
                            queue.borrow_mut().push(json);
                        }
                    }
                    Line::Closed => {
                        // The compositor closed the socket, which means the
                        // session is over. Staying up would leave a browser
                        // window showing a desktop that no longer exists.
                        tracing::info!("the compositor closed the socket; stopping");
                        app.quit();
                        return;
                    }
                }
            }
        });
    }

    Ok(())
}

enum Line {
    Event(String),
    Closed,
}

/// Deliver one event to the page.
///
/// The script is built by `viewport_ipc::js`, which is also what the in-process
/// WPE backend evaluates — the shell must not be able to tell which engine is
/// underneath it from the message it receives.
fn post(view: &webkit6::WebView, json: &str) {
    view.evaluate_javascript(
        &viewport_ipc::js::dispatch(json),
        None,
        None,
        gtk::gio::Cancellable::NONE,
        |_| {},
    );
}

/// Whether a line is the compositor asking for a reload.
///
/// Matched on the wire rather than deserialised into `viewport_ipc::Event`:
/// every other line is destined for the page unexamined, and parsing them all
/// to recognise one would mean this process rejecting messages the page would
/// have understood.
fn is_reload(json: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Typed<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
    }
    matches!(
        serde_json::from_str::<Typed>(json),
        Ok(Typed {
            kind: "shell.reload"
        })
    )
}
