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

use std::rc::Rc;

use anyhow::{anyhow, Result};
use gtk4 as gtk;
use viewport_shell_bridge::{Line, Options};
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

/// How many more reloads a degraded shell gets once the crash limit is past.
///
/// Degraded is the end of the line — there is no further mode to fall back to
/// — so these are the last tries, spent slowly rather than all at once: a web
/// process that dies the moment it is given a page will die again however
/// often it is handed one, and reloading in a tight loop is a
/// WebKitWebProcess spawn storm wearing a shell as a coat.
const DEGRADED_RELOAD_LIMIT: u32 = 5;

/// The first degraded reload's wait, doubled for each one after it.
///
/// The same shape as the compositor's own restart backoff
/// (`shell_client::restart_backoff`), for the same reason: repeated fast
/// failures are a fault that trying again *right now* will not fix.
const DEGRADED_RELOAD_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// And as far apart as the compositor spreads its own restarts.
const DEGRADED_RELOAD_CAP: std::time::Duration = std::time::Duration::from_secs(30);

/// The exit status of a shell that has used up every try it had.
///
/// The compositor reads this and takes the slot down rather than restarting
/// it — see `gave_up` in shell_client — which is the point of exiting rather
/// than reloading forever: handing the decision over to a supervisor that
/// knows what the code means is what turns an unbounded crash-and-reload
/// storm into a shell that stays down until a human changes something.
/// Distinct from [`RETRY_WITHOUT_DMABUF`] so the two decisions can be told
/// apart in a log or a wait status; the number is arbitrary beyond that.
const DEGRADED_EXHAUSTED: i32 = 88;

/// How many events may wait for a page that has not committed yet.
///
/// A few hundred covers any ordinary load with room to spare — the queue
/// exists for the second or two before Committed — and a ceiling is what
/// keeps it from being a second, unbounded copy of everything the desktop
/// does when the page never arrives at all.
const QUEUE_LIMIT: usize = 512;

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
    //
    // Unless the compositor says there is something back there — a terminal
    // drawn as the wallpaper — in which case this colour is precisely what
    // would cover it, and the page composites over nothing instead. The white
    // flash is traded for the desktop being briefly see-through while the page
    // loads, which is the right way round: one is a bug and the other is what
    // was asked for.
    let behind = std::env::var_os("VIEWPORT_SHELL_TRANSPARENT").is_some();
    view.set_background_color(if behind {
        &gtk::gdk::RGBA::TRANSPARENT
    } else {
        &gtk::gdk::RGBA::BLACK
    });

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
    //
    // Degraded is not a licence to reload forever, which is how it used to be
    // read: past the crash limit a degraded shell reloaded on every
    // termination without end, so one broken GPU meant WebKitWebProcesses
    // spawned back to back for as long as the session lived. There the tries
    // are counted and spread out, and when they run out the process says so
    // loudly and exits — see `DEGRADED_RELOAD_LIMIT` and `DEGRADED_EXHAUSTED`.
    {
        let crashes = std::cell::Cell::new(0u32);
        let degraded_reloads = std::cell::Cell::new(0u32);
        let degraded = std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_some();
        view.connect_web_process_terminated(move |view, reason| {
            let count = crashes.get() + 1;
            crashes.set(count);
            tracing::error!("the web process died ({reason:?}), {count} time(s)");
            if count < WEB_PROCESS_CRASH_LIMIT {
                view.reload();
                return;
            }
            if !degraded {
                tracing::error!(
                    "it has died {count} times; asking to be started again with WebKit's \
                     DMA-BUF renderer off"
                );
                std::process::exit(RETRY_WITHOUT_DMABUF);
            }

            // The last resort, spent slowly. Each wait doubles, so five tries
            // stretch over about a minute instead of one busy loop.
            let attempt = degraded_reloads.get() + 1;
            degraded_reloads.set(attempt);
            if attempt > DEGRADED_RELOAD_LIMIT {
                tracing::error!(
                    "it has died {count} times with WebKit's DMA-BUF renderer already off, \
                     and {DEGRADED_RELOAD_LIMIT} slow reloads since have each died too; \
                     there is nothing further to try from here"
                );
                std::process::exit(DEGRADED_EXHAUSTED);
            }
            let delay = DEGRADED_RELOAD_BACKOFF
                .checked_mul(1u32 << (attempt - 1))
                .unwrap_or(DEGRADED_RELOAD_CAP)
                .min(DEGRADED_RELOAD_CAP);
            tracing::error!(
                "it has died {count} times with WebKit's DMA-BUF renderer already off; \
                 reloading in {}s (degraded reload {attempt}/{DEGRADED_RELOAD_LIMIT})",
                delay.as_secs()
            );
            // Waited out on the main loop rather than slept through: this
            // handler runs on the GTK thread, and a process that cannot render
            // still owns the window everyone can see.
            let view = view.clone();
            gtk::glib::timeout_add_seconds_local(delay.as_secs().max(1) as u32, move || {
                view.reload();
                gtk::glib::ControlFlow::Break
            });
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

    // The window under the page, which is opaque by default and is *not* the
    // same thing as the web view's background colour.
    //
    // Setting the view's background to transparent only stops WebKit painting
    // one; what shows through then is GTK's own window background — Adwaita's
    // #353535 — and that covered the wallpaper terminal just as thoroughly as
    // black would have. Both have to go, and the window's is CSS.
    if behind {
        let css = gtk::CssProvider::new();
        // `load_from_data`, not `load_from_string`: the latter needs the v4_12
        // feature on the gtk4 crate, and this backend builds against whatever
        // GTK the system has.
        css.load_from_data("window, window > * { background: transparent; }");
        gtk::style_context_add_provider_for_display(
            &gtk::prelude::WidgetExt::display(&window),
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    bridge(&view, &manager, options, app)?;

    view.load_uri(&options.url);
    window.present();
    Ok(())
}

/// Wire the page to the compositor, in both directions.
///
/// The socket, its two threads and the framing are
/// `viewport_shell_bridge`; what is here is the part that is WebKit's — the
/// script message handler outbound, and an evaluated script inbound.
fn bridge(
    view: &webkit6::WebView,
    manager: &webkit6::UserContentManager,
    options: &Options,
    app: &gtk::Application,
) -> Result<()> {
    // The reader thread cannot touch the view, so lines cross onto the GTK
    // main context here. glib's own channel was removed in 0.18; this is what
    // the gtk-rs book replaced it with.
    let (in_tx, in_rx) = async_channel::unbounded::<Line>();
    let out = viewport_shell_bridge::connect(&options.socket, move |line| {
        // A closed channel means the loop below has already stopped, which is
        // to say the process is on its way out.
        let _ = in_tx.send_blocking(line);
    })?;

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
        out.send(json);
    });

    // Events that arrived before the page could receive them.
    //
    // The compositor starts broadcasting the moment it accepts the connection,
    // and the connection is made before the page has finished loading —
    // deliberately, so nothing that happens during the load is missed. A
    // script evaluated against a document that does not exist yet is dropped
    // on the floor, so they wait here instead.
    //
    // Bounded, because "before the page has finished loading" has no natural
    // end: a page that never reaches Committed — a load failure, a web
    // process dying before it got there — would otherwise queue everything
    // the desktop did for as long as the shell was up. Past the limit the
    // oldest event goes, which is the one the page would make least of.
    let queue: Rc<std::cell::RefCell<std::collections::VecDeque<String>>> =
        Rc::new(Default::default());
    let loaded = Rc::new(std::cell::Cell::new(false));
    let dropped = Rc::new(std::cell::Cell::new(0usize));

    {
        let queue = queue.clone();
        let loaded = loaded.clone();
        let dropped = dropped.clone();
        view.connect_load_changed(move |view, event| match event {
            webkit6::LoadEvent::Committed => {
                // Committed, not Finished: the document and its scripts exist
                // here, and waiting for every subresource would hold the
                // desktop's state back behind a slow image.
                loaded.set(true);
                let dropped = dropped.replace(0);
                for json in queue.borrow_mut().drain(..) {
                    post(view, &json);
                }
                // Said here rather than per drop: a busy desktop behind a page
                // that never commits sheds hundreds of events a second, and
                // each line would be the same line.
                if dropped > 0 {
                    tracing::warn!(
                        "the page committed after {dropped} queued event(s) had been dropped"
                    );
                }
            }
            webkit6::LoadEvent::Started => {
                // A reload throws the page away, so anything queued for the
                // old one is stale and anything sent to the new one has to
                // wait for it.
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
                        if viewport_shell_bridge::is_reload(&json) {
                            tracing::info!("reloading the shell");
                            view.reload_bypass_cache();
                            continue;
                        }
                        if loaded.get() {
                            post(&view, &json);
                        } else {
                            let mut queue = queue.borrow_mut();
                            if queue.len() >= QUEUE_LIMIT {
                                queue.pop_front();
                                dropped.set(dropped.get() + 1);
                            }
                            queue.push_back(json);
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
