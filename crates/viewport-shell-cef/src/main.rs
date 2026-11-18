// SPDX-License-Identifier: MIT
//
// The shell, out of process, rendered by Chromium embedded through CEF.
//
// The difference from `viewport-shell-chromium`, which runs the same engine:
// that one starts a browser and talks to it over a socket, this one *is* the
// browser. CEF is linked in, the window is created through CEF's Views
// framework, and the page is reached through the library rather than through a
// protocol — no browser binary, no DevTools pipe, one process fewer.
//
// Which matters for two reasons beyond tidiness. It is a fourth measurement of
// the same shell, with the same engine as the third and a different
// architecture around it. And it is the only route to offscreen rendering:
// CEF's `OnAcceleratedPaint` hands over DMA-BUF planes, a modifier and a
// format — very nearly `viewport_web::Frame` — which is what the shell element
// takes directly. That is the version worth having in the compositor process,
// and this is the step before it.
//
// CEF re-executes this binary for its zygote, GPU and renderer processes, so
// `execute_process` has to run before anything else does and exit when it
// returns a code. Everything below that line runs in the browser process only.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{c_char, CString};
use std::io::Read as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Context as _, Result};
use cef::{args::Args, rc::*, *};
use serde_json::{json, Value};

use viewport_shell_bridge::{DocumentGuard, Line, Options};

/// The name the page calls to reach the compositor, wrapped by
/// `viewport_ipc::js::BRIDGE_SHIM` in the name the shell was written against.
const BINDING: &str = "__viewport_send";

/// The compositor, for the DevTools observer to hand messages to.
///
/// A static because the observer is built by CEF and there is nowhere else to
/// put it: it is `Send`, unlike everything else here.
static OUT: OnceLock<viewport_shell_bridge::Sender> = OnceLock::new();

/// Events that arrived before the page could take them.
///
/// The compositor starts talking the moment it accepts the connection, which
/// is before CEF has a browser, let alone a document. A script evaluated
/// against a page that does not exist is dropped on the floor. A ceiling is
/// what keeps that from being a second copy of everything the desktop does
/// when the page never arrives at all.
static QUEUE: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// The document the bridge speaks to: which frame and context may talk, and
/// whether queued events may be evaluated into it yet.
///
/// The gate half exists because of a reload bug this backend had: the
/// reload's own `Page.reload` method result flowed through
/// `on_dev_tools_method_result` microseconds after the gate was closed, the
/// guard's URL was still the old document's (known allowed), and the gate
/// swapped itself open — draining the queue into the document that was going
/// away and leaving every event that arrived during the reload to be
/// evaluated the same way. The load event is the one thing that says the
/// document the events are for exists.
///
/// The rule is the shared one now (`viewport_shell_bridge::DocumentGuard`):
/// only `Page.loadEventFired` opens the gate; navigation and context
/// bookkeeping close it; method results never open it.
static DOCUMENT: Mutex<DocumentGuard> = Mutex::new(DocumentGuard::new());

/// A reload that arrived before there was a browser to send it to.
///
/// The socket reader posts a `Deliver` task for every event, and the task's
/// `execute` has no browser until the window exists. A reload is a control
/// message, not page data; queueing it in `QUEUE` would have it evaluated as a
/// `CustomEvent` when the page finally arrives, where nothing handles it.
/// Remembered here and replayed at the end of `install_bridge`.
static PENDING_RELOAD: AtomicBool = AtomicBool::new(false);

/// `Deliver` tasks posted to CEF's UI thread but not yet run.
///
/// CEF's own task queue has no ceiling, so the `QUEUE` cap below only bounds
/// what has already reached the UI thread; a socket reader faster than a UI
/// thread busy with a long evaluate could grow CEF's queue and its copies of
/// every event without limit. Counted before `post_task` so the ceiling
/// applies to messages in flight too.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Whether a dropped `Deliver` task has been logged already.
static TASK_DROP_WARNED: AtomicBool = AtomicBool::new(false);

/// Whether a refused page-to-compositor message has already been logged.
static DROP_WARNED: AtomicBool = AtomicBool::new(false);

/// How many events may wait for a page that never arrives.
const QUEUE_LIMIT: usize = 512;

/// DevTools command ids. Any monotonic sequence will do; nothing here waits on
/// a reply, and the ids only have to be distinct.
static NEXT_ID: AtomicI64 = AtomicI64::new(1);

thread_local! {
    /// The browser, and the registration that keeps the observer alive.
    ///
    /// Thread-local rather than static because neither is `Send` — CEF objects
    /// belong to the thread that made them, which for both of these is the UI
    /// thread. Everything that touches them arrives there through `post_task`.
    static BROWSER: RefCell<Option<Browser>> = const { RefCell::new(None) };
    static OBSERVER: RefCell<Option<Registration>> = const { RefCell::new(None) };
}

fn main() -> Result<()> {
    // Before the logger, before the options, before anything: in a subprocess
    // this call does the subprocess's whole job, and whatever this program
    // would otherwise do at startup would be done once per CEF process.
    // Before any CEF type is built, in every process.
    //
    // The library checks an API version on each structure it is handed, and
    // that version is only set once this has run. Without it the first call
    // into CEF dies with "CefApp_0_CToCpp called with invalid version -1",
    // which names neither the missing call nor the process it was missing in.
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = Args::new();
    let code = execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());
    if code >= 0 {
        std::process::exit(code);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let argv: Vec<String> = std::env::args().collect();
    let options = Options::parse(&argv)?;

    // The compositor refuses a remote shell before starting this process; this
    // is the same policy for a shell started by hand.
    anyhow::ensure!(
        viewport_ipc::js::shell_url_allowed(&options.url),
        "refusing to load a non-loopback shell from {}; set {}=1 to allow a remote origin",
        options.url,
        viewport_ipc::js::ALLOW_REMOTE_SHELL_ENV
    );

    // A cache directory of this process's own.
    //
    // Left unset, CEF uses one shared path for every instance and warns that
    // it "may lead to unintended process singleton behavior" — which is not a
    // warning about tidiness. Chromium's singleton lock lives in that
    // directory, so a second shell started while a desktop is running finds
    // the lock held: it hands its command line to the running instance and
    // exits, or takes the lock from it. Either way one of the two stops being
    // a working desktop, and what surfaced here was `CEF would not initialise`
    // in the new one and a live session that had to be restarted.
    //
    // The chromium backend has had a per-process profile from the start
    // (`--user-data-dir`), which is why it can be run beside a live session
    // and this could not.
    //
    // Mode 0700 at a random suffix rather than a pid-only path with
    // `create_dir_all`: a predictable name in `/tmp` is something another
    // local user can pre-create, symlink or read, and this directory holds
    // the renderer's profile and lock.
    let cache = private_profile_dir("viewport-shell-cef")?;

    let settings = Settings {
        // No sandbox: the helper is a setuid binary and a nix store path
        // cannot be one. That is a real reduction in process isolation and is
        // kept deliberately, because removing the switch does not make
        // Nix-built CEF able to sandbox; it makes it fail.
        //
        // The compensating controls are above and below: the shell URL must be
        // a file: or loopback origin unless `VIEWPORT_ALLOW_REMOTE_SHELL=1`,
        // the bridge refuses subframe execution contexts, and a navigation to
        // an origin outside the policy revokes the bridge. A page from the
        // open web is not loaded by default, so the unsandboxed renderer does
        // not get one.
        no_sandbox: 1,
        root_cache_path: CefString::from(cache.to_string_lossy().as_ref()),
        cache_path: CefString::from(cache.to_string_lossy().as_ref()),
        ..Default::default()
    };

    // Whatever else this session wants on the browser process's command line,
    // the way the `chromium` backend already takes `VIEWPORT_CHROMIUM_ARGS`.
    //
    // The switch that prompted it is `--force-renderer-accessibility`. Blink
    // builds its accessibility tree lazily and publishes it over AT-SPI once
    // it notices a screen reader on the bus, which is the behaviour to want —
    // a tree built for a session nobody is reading costs memory in every
    // renderer for nothing. But "once it notices" is a negotiation with a bus
    // that a compositor started from a TTY may not have, and when that does
    // not happen there is nothing in any log that says so. This is the lever
    // to pull to find out, and it is the same lever the other Blink backend
    // has always had; the only reason CEF did not is that there was nowhere to
    // put it. See docs/shell-backends.md.
    //
    // Declared before `extra` and not beside the push, because `extra` borrows
    // these strings and `Argv` borrows `extra`: a `Vec<String>` built where it
    // is used is dropped before the thing pointing into it.
    let requested: Vec<String> = std::env::var("VIEWPORT_CEF_ARGS")
        .into_iter()
        .flat_map(|s| {
            s.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<String>>()
        })
        .collect();

    // The switches the browser process needs and the command line does not
    // carry. Appended rather than required of whoever starts this, because a
    // shell that has to be launched with four flags to work at all is a shell
    // that will be launched without them.
    //
    //   --ozone-platform=wayland  CEF defaults to X11 on Linux, and under this
    //                             compositor that means Xwayland — where the
    //                             window would be a client of a client and not
    //                             the desktop.
    //   --no-sandbox              the helper is a setuid binary and a nix store
    //                             path cannot be one. Kept because dropping
    //                             it does not add a working sandbox on this
    //                             build; see the Settings comment for the
    //                             origin/top-frame controls that carry the
    //                             security posture instead.
    //   --in-process-gpu          with a GPU process of its own, Chromium
    //                             segfaults on this compositor and falls back
    //                             to software, which is a buffer the shell
    //                             element cannot draw. The same thing the
    //                             chromium backend does, for the same reason.
    //   --enable-logging=stderr   or Chromium logs to a file nobody looks in,
    //                             and a CHECK failure reads as SIGTRAP and
    //                             nothing else.
    let mut extra = vec![
        "--ozone-platform=wayland",
        "--no-sandbox",
        "--enable-logging=stderr",
    ];
    if std::env::var_os("VIEWPORT_CHROMIUM_GPU_PROCESS").is_none() {
        extra.push("--in-process-gpu");
    }
    extra.extend(requested.iter().map(String::as_str));
    let browser_args = Argv::new(&argv, &extra)?;

    // The page is opened from `on_context_initialized` rather than from here.
    // Views wants a context to build in, and `initialize` returning only means
    // the browser process is starting one — a window created between those two
    // points takes the process down with a trap and no message, about thirteen
    // seconds later, which is a long way from the line that caused it.
    let mut app = ShellApp::new(options.url.clone());
    if initialize(
        Some(browser_args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    ) != 1
    {
        return Err(anyhow!("CEF would not initialise"));
    }

    // Both directions, started before the loop: the socket is what the shell
    // is for, and a page that came up before the connection did would have
    // nothing to talk to.
    let out = viewport_shell_bridge::connect(&options.socket, |line| match line {
        Line::Event(json) => {
            // Onto CEF's UI thread. This closure runs on the socket reader,
            // and a `Browser` may not leave the thread that made it.
            post_deliver(json);
        }
        Line::Closed => {
            tracing::info!("the compositor closed the socket; stopping");
            let mut task = Stop::new();
            post_task(ThreadId::UI, Some(&mut task));
        }
    })?;
    let _ = OUT.set(out);

    tracing::info!("CEF is up on {}", options.url);
    run_message_loop();
    shutdown();
    // Thrown away with the process, like the chromium backend's profile: what
    // is in it is a cache for a page this compositor ships.
    let _ = std::fs::remove_dir_all(&cache);
    Ok(())
}

// The macro takes the three delegate interfaces a window delegate is made of,
// in the order CEF declares them: a view, a panel, and the window itself. The
// first two have nothing to say here and are empty rather than absent.
/// A command line for CEF, owned for as long as CEF is reading it.
///
/// `cef::args::Args` builds one from this process's arguments and there is no
/// way to add to it, so this is the same thing with room for the switches the
/// browser process needs. The two vectors are the storage `MainArgs` points
/// into: dropping them while CEF is running would leave it reading freed
/// memory, which is why they are held rather than built inline.
struct Argv {
    _owned: Vec<CString>,
    _pointers: Vec<*const c_char>,
    main_args: MainArgs,
}

impl Argv {
    fn new(argv: &[String], extra: &[&str]) -> Result<Self> {
        let owned: Vec<CString> = argv
            .iter()
            .map(String::as_str)
            .chain(extra.iter().copied())
            .map(CString::new)
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| anyhow!("an argument contains a nul byte: {e}"))?;
        let pointers: Vec<*const c_char> = owned.iter().map(|arg| arg.as_ptr()).collect();
        let main_args = MainArgs {
            argc: pointers.len() as i32,
            argv: pointers.as_ptr() as *mut *mut _,
        };
        Ok(Self {
            _owned: owned,
            _pointers: pointers,
            main_args,
        })
    }

    fn as_main_args(&self) -> &MainArgs {
        &self.main_args
    }
}

wrap_app! {
    struct ShellApp {
        url: String,
    }

    impl App {
        /// The browser process is ready. This is the only place a Views window
        /// may be made.
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(ShellProcess::new(self.url.clone()))
        }
    }
}

wrap_browser_process_handler! {
    struct ShellProcess {
        url: String,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            // Whether anything is drawn behind the page.
            //
            // Chromium composites the document over a colour of its own, and
            // the default is opaque white — so with a terminal drawn as the
            // wallpaper, that colour is exactly what covers it. Zero is CEF's
            // "fully transparent", which is what turns transparent painting on
            // (`cef_browser_settings_t::background_color`).
            //
            // The window gets the same treatment in `on_window_created`: the
            // browser view painting nothing only reveals whatever the Views
            // window paints, which is opaque as well.
            let behind = std::env::var_os("VIEWPORT_SHELL_TRANSPARENT").is_some();
            let settings = BrowserSettings {
                background_color: if behind { 0 } else { BrowserSettings::default().background_color },
                ..Default::default()
            };
            let Some(view) = browser_view_create(
                None,
                Some(&CefString::from(self.url.as_str())),
                Some(&settings),
                None,
                None,
                None,
            ) else {
                tracing::error!("CEF would not make a browser view");
                return;
            };
            let mut delegate = ShellWindow::new(view);
            if window_create_top_level(Some(&mut delegate)).is_none() {
                tracing::error!("CEF would not make a window");
            }
        }
    }
}

wrap_window_delegate! {
    struct ShellWindow {
        view: BrowserView,
    }

    impl ViewDelegate {
    }

    impl PanelDelegate {
    }

    impl WindowDelegate {
        /// The window exists; put the page in it and show it.
        fn on_window_created(&self, window: Option<&mut Window>) {
            let Some(window) = window else { return };
            // `From<&BrowserView>` rather than `From<BrowserView>`: the
            // conversion is a cast between two views of the same refcounted
            // object, not a move.
            let mut child = View::from(&self.view);
            // Transparent all the way down, when something is behind the page.
            //
            // Three things paint here — the document, the browser view and the
            // Views window — and any one of them left opaque is a wallpaper
            // nobody can see. The document is the shell's own CSS, the browser
            // view is `BrowserSettings::background_color` above, and this is
            // the third.
            if std::env::var_os("VIEWPORT_SHELL_TRANSPARENT").is_some() {
                child.set_background_color(0);
                window.set_background_color(0);
            }
            window.add_child_view(Some(&mut child));
            window.show();
            // The desktop is the whole layout, and CEF's Views window comes up
            // at its own default of 800x600 and stays there — the compositor's
            // configure sizes the surface and Chromium keeps drawing the size
            // it decided on, so the shell would be a small patch in a corner.
            // Fullscreen is the state the compositor already puts this
            // toplevel in; this is the window agreeing to it.
            window.set_fullscreen(1);

            // Only now: the browser is made when its view is put in a window,
            // so asking before this point answers `None`.
            let Some(browser) = self.view.browser() else {
                tracing::error!("the browser view has no browser; there is no bridge");
                return;
            };
            install_bridge(&browser);
            BROWSER.with(|slot| *slot.borrow_mut() = Some(browser));
        }

        /// No titlebar and no border: this window is the desktop, and what
        /// goes around a window is drawn by the compositor.
        fn is_frameless(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            1
        }

        /// Resizable, which is not the default here and has to be said.
        ///
        /// Every method of this delegate that is not implemented answers
        /// `Default::default()`, which for a `c_int` is 0 — so a delegate that
        /// says nothing says the window cannot be resized. Chromium then tells
        /// the compositor its minimum and maximum size are both 800x600 and
        /// ignores the configure, and the desktop is drawn 800x600 in the
        /// corner of the screen whatever the layout is.
        fn can_resize(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            1
        }

        /// The states a desktop can legitimately be in. Maximise and minimise
        /// are refused for the same reason the frame is: this window is not
        /// one the user manages.
        fn can_maximize(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            0
        }

        fn can_minimize(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            0
        }

        /// Closable, so that a compositor going away closes the window rather
        /// than leaving a browser holding a dead connection.
        fn can_close(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            1
        }
    }
}

/// Put the page-to-compositor half of the bridge in place.
///
/// The same three DevTools calls the chromium backend makes, without its
/// target discovery: there, the protocol reaches a browser that has many
/// pages; here the host *is* the page.
fn install_bridge(browser: &Browser) {
    let observer = ShellObserver::new();
    let registration = browser
        .host()
        .and_then(|host| host.add_dev_tools_message_observer(Some(&mut observer.clone())));
    if registration.is_none() {
        tracing::error!("CEF would not take a devtools observer; there is no bridge");
        return;
    }
    // Kept, because dropping it removes the observer.
    OBSERVER.with(|slot| *slot.borrow_mut() = registration);

    // Page first, so the frame tree is in place before execution contexts
    // arrive; `Runtime.enable` is what starts those.
    send(browser, &json!({"id": next_id(), "method": "Page.enable"}));
    // The reply names the main frame. Execution contexts carry a frame id,
    // and a subframe's default context is not the shell's.
    send(
        browser,
        &json!({"id": next_id(), "method": "Page.getFrameTree"}),
    );
    send(
        browser,
        &json!({"id": next_id(), "method": "Runtime.enable"}),
    );
    // The outbound half: a real function in the page that calls back out here.
    send(
        browser,
        &json!({
            "id": next_id(),
            "method": "Runtime.addBinding",
            "params": {"name": BINDING},
        }),
    );
    // The name the shell reaches for, wrapped around it, before any of the
    // page's own scripts run — `data/shell/state.js` reads the handler at load
    // time, so arriving late is the same as not arriving.
    send(
        browser,
        &json!({
            "id": next_id(),
            "method": "Page.addScriptToEvaluateOnNewDocument",
            "params": {"source": viewport_ipc::js::BRIDGE_SHIM},
        }),
    );
    // And once for the document that is already loading, which the line above
    // is too late for.
    evaluate(browser, viewport_ipc::js::BRIDGE_SHIM);

    // The frame bookkeeping arrives through the observer, not here, so the
    // URL is likely still unknown: the gate starts shut, and only the
    // document's own `Page.loadEventFired` opens it. Queued events wait for
    // that load event rather than being evaluated into a document whose
    // origin — and whose listener — is not yet known; the first install
    // drains on the load event now, not here.
    if let Ok(mut doc) = DOCUMENT.lock() {
        let url = doc.guard.main_url.clone();
        doc.gate.frame_navigated(url.as_deref());
    }
    if PENDING_RELOAD.swap(false, Ordering::SeqCst) {
        // Asked for before the page existed. The shim above is installed, so
        // the document this reload produces has it; the queued events wait for
        // its load event rather than going into the document being replaced.
        tracing::info!("reloading the shell as soon as its page exists");
        if let Ok(mut doc) = DOCUMENT.lock() {
            doc.gate.reload_requested();
        }
        send(
            browser,
            &json!({
                "id": next_id(),
                "method": "Page.reload",
                "params": {"ignoreCache": true},
            }),
        );
    }
}

/// A fresh directory only this process can enter, under `/tmp`.
///
/// `DirBuilder::mode(0o700).create` is what makes the name safe: it fails when
/// anything already exists there, so a directory or symlink another local user
/// made at that path cannot be adopted. The random suffix is retried on
/// collision.
fn private_profile_dir(prefix: &str) -> Result<std::path::PathBuf> {
    for _ in 0..8 {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", random_suffix()?));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }
    Err(anyhow!(
        "could not create a private {prefix} directory under {}",
        std::env::temp_dir().display()
    ))
}

/// 64 random bits, as hex, from the kernel's own source.
fn random_suffix() -> Result<String> {
    let mut bytes = [0u8; 8];
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom")?
        .read_exact(&mut bytes)
        .context("reading /dev/urandom")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn next_id() -> i64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Post one event to CEF's UI thread, bounded like the queue it feeds.
///
/// `enqueue` only runs once a `Deliver` task is on the UI thread; CEF's task
/// queue is unbounded, so the ceiling has to be here, before `post_task`.
fn post_deliver(json: String) {
    if IN_FLIGHT.fetch_add(1, Ordering::AcqRel) >= QUEUE_LIMIT {
        IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        if !TASK_DROP_WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!("the shell's incoming task queue is full; dropping a message");
        }
        return;
    }
    let mut task = Deliver::new(json);
    if post_task(ThreadId::UI, Some(&mut task)) == 0 {
        IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        if !TASK_DROP_WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!("CEF refused the shell's incoming task queue; dropping a message");
        }
    }
}

/// Park an event until the page can take it, bounded like the gtk shell's
/// queue: past the limit, the oldest goes.
fn enqueue(json: String) {
    if let Ok(mut queue) = QUEUE.lock() {
        if queue.len() >= QUEUE_LIMIT {
            queue.pop_front();
        }
        queue.push_back(json);
    }
}

/// One DevTools message to the page's own agent.
fn send(browser: &Browser, message: &Value) {
    let Some(host) = browser.host() else { return };
    match serde_json::to_vec(message) {
        Ok(bytes) => {
            host.send_dev_tools_message(Some(&bytes));
        }
        Err(e) => tracing::error!("could not encode a devtools command: {e}"),
    }
}

fn evaluate(browser: &Browser, script: &str) {
    send(
        browser,
        &json!({
            "id": next_id(),
            "method": "Runtime.evaluate",
            "params": {"expression": script, "awaitPromise": false, "returnByValue": false},
        }),
    );
}

/// Wrap a DevTools event as the frame bookkeeping `DocumentGuard::note`
/// folds, or `None` when it is not bookkeeping.
///
/// Only the three methods that move the document are wrapped; everything
/// else a page can flood — binding calls, load events, console output — is
/// answered `None` without its parameters being parsed. Malformed params are
/// `None` too: bookkeeping that cannot be read is not noted, which fails
/// closed rather than guessing what it said.
fn bookkeeping_event(method: &str, params: &[u8]) -> Option<Value> {
    match method {
        "Page.frameNavigated" | "Runtime.executionContextCreated" => {
            let params = serde_json::from_slice::<Value>(params).ok()?;
            Some(json!({"method": method, "params": params}))
        }
        "Runtime.executionContextsCleared" => Some(json!({"method": method})),
        _ => None,
    }
}

/// Wrap a `Page.getFrameTree` method result as the bookkeeping
/// `DocumentGuard::note` folds, or `None` when the result is not a frame tree
/// or cannot be read. The result wrapper is the same `{"frameTree": ...}`
/// shape CDP returns from the method.
fn frame_tree_result(result: &[u8]) -> Option<Value> {
    let result = serde_json::from_slice::<Value>(result).ok()?;
    result.pointer("/frameTree/frame")?;
    Some(json!({"result": result}))
}

wrap_dev_tools_message_observer! {
    struct ShellObserver;

    impl DevToolsMessageObserver {
        /// An event from the page's agent. Two matter: a document that has
        /// finished loading, which lets messages flow again, and the binding
        /// being called, which is the page talking.
        fn on_dev_tools_event(
            &self,
            browser: Option<&mut Browser>,
            method: Option<&CefString>,
            params: Option<&[u8]>,
        ) {
            let method = method.map(CefString::to_string);
            // Track the main frame and its execution context before deciding
            // anything about a message. This is what lets `bindingCalled`
            // below tell the shell document from an iframe. The gate wiring
            // rides the note: bookkeeping closes the gate, never opens it.
            if let (Some(method), Some(params)) = (method.as_deref(), params) {
                if let Some(wrapped) = bookkeeping_event(method, params) {
                    if let Ok(mut doc) = DOCUMENT.lock() {
                        doc.note(&wrapped);
                    }
                }
            }
            // A document was replaced — a reload, or the shell navigating. The
            // shim is re-installed by `addScriptToEvaluateOnNewDocument` before
            // any of its scripts run, so this only has to let messages flow
            // again: the same signal the chromium backend waits on.
            if method.as_deref() == Some("Page.loadEventFired") {
                // The document is up; this is the one thing that opens the
                // gate. Both answers are read under one lock, released before
                // the queue is touched.
                let (ready, url_known) = DOCUMENT
                    .lock()
                    .map(|mut doc| (doc.load_event_fired(), doc.guard.main_url.is_some()))
                    .unwrap_or((false, false));
                let present = BROWSER.with(|slot| slot.borrow().is_some());
                // And hand over what arrived while the document was loading.
                // `install_bridge` no longer drains at all, so without this
                // every event sent across a reload stayed in the queue for
                // ever.
                if present && ready {
                    let waiting: Vec<String> = QUEUE
                        .lock()
                        .map(|mut queue| queue.drain(..).collect())
                        .unwrap_or_default();
                    if let Some(browser) = browser {
                        for json in waiting {
                            evaluate(browser, &viewport_ipc::js::dispatch(&json));
                        }
                    }
                } else if url_known {
                    // The URL is known and the gate is shut: the shell left
                    // the allowed origin, so the queue goes — the chromium
                    // backend's drop branch.
                    let dropped = QUEUE
                        .lock()
                        .map(|mut queue| {
                            let dropped = queue.len();
                            queue.clear();
                            dropped
                        })
                        .unwrap_or(0);
                    if dropped > 0 {
                        tracing::warn!(
                            "dropping queued compositor events: the shell left the allowed origin"
                        );
                    }
                }
                return;
            }
            if method.as_deref() != Some("Runtime.bindingCalled") {
                return;
            }
            let Some(params) = params else { return };
            let Ok(params) = serde_json::from_slice::<Value>(params) else {
                tracing::warn!("undecodable bindingCalled parameters");
                return;
            };
            // A page can add bindings of its own, and a compositor that
            // forwarded them would be taking instructions from whatever the
            // page felt like naming.
            if params.get("name").and_then(Value::as_str) != Some(BINDING) {
                return;
            }
            let Some(payload) = params.get("payload").and_then(Value::as_str) else {
                return;
            };
            let context = params.get("executionContextId").and_then(Value::as_i64);
            let allowed = match DOCUMENT.lock() {
                Ok(doc) => doc.guard.context_allows(context) && doc.guard.url_allows(),
                Err(_) => false,
            };
            if !allowed {
                // Once, not per message: a subframe that keeps trying must
                // not become the log.
                if !DROP_WARNED.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "dropping a page-to-compositor message: it did not come from \
                         the top frame's execution context, or the main frame's URL \
                         is not a file:/loopback origin; set {}=1 to allow a remote shell",
                        viewport_ipc::js::ALLOW_REMOTE_SHELL_ENV
                    );
                }
                return;
            }
            if let Some(out) = OUT.get() {
                out.send(payload.to_owned());
            }
        }

        /// A DevTools command result. `Page.getFrameTree` is the one whose
        /// result names the main frame; the rest are not our business.
        ///
        /// The gate wiring rides the note — bookkeeping closes the gate —
        /// and nothing here opens it: `Page.reload`'s own result arrives
        /// while the old document is still up, and opening on it was how a
        /// reload drained its queue into a page that was going away.
        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut Browser>,
            _message_id: ::std::os::raw::c_int,
            success: ::std::os::raw::c_int,
            result: Option<&[u8]>,
        ) {
            if success == 0 {
                return;
            }
            let Some(result) = result else {
                return;
            };
            let Some(wrapped) = frame_tree_result(result) else {
                return;
            };
            if let Ok(mut doc) = DOCUMENT.lock() {
                doc.note(&wrapped);
            }
        }
    }
}

wrap_task! {
    struct Deliver {
        json: String,
    }

    impl Task {
        /// On the UI thread, where the browser may be touched.
        fn execute(&self) {
            IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
            BROWSER.with(|slot| {
                let slot = slot.borrow();
                let Some(browser) = slot.as_ref() else {
                    if viewport_shell_bridge::is_reload(&self.json) {
                        // No browser to send a reload command to yet. A
                        // reload is not page data, so it cannot go in the
                        // queue; `install_bridge` replays it.
                        PENDING_RELOAD.store(true, Ordering::SeqCst);
                        return;
                    }
                    enqueue(self.json.clone());
                    return;
                };
                if viewport_shell_bridge::is_reload(&self.json) {
                    tracing::info!("reloading the shell");
                    // Held shut until the new document announces itself with
                    // `Page.loadEventFired`, which the DevTools observer
                    // watches for: events that arrive meanwhile would land in
                    // a document that is going away and never be seen again.
                    // The reload's own method result flows through the result
                    // path above, which only closes the gate further — it
                    // cannot reopen it.
                    if let Ok(mut doc) = DOCUMENT.lock() {
                        doc.gate.reload_requested();
                    }
                    send(browser, &json!({
                        "id": next_id(),
                        "method": "Page.reload",
                        "params": {"ignoreCache": true},
                    }));
                    return;
                }
                let ready = DOCUMENT.lock().map(|doc| doc.gate.ready()).unwrap_or(false);
                if ready {
                    evaluate(browser, &viewport_ipc::js::dispatch(&self.json));
                } else {
                    enqueue(self.json.clone());
                }
            });
        }
    }
}

wrap_task! {
    struct Stop;

    impl Task {
        fn execute(&self) {
            quit_message_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The adapter shapes round-trip into the shared guard: a wrapped event
    /// is exactly what `DocumentGuard::note` folds.
    #[test]
    fn a_frame_navigation_round_trips_into_bookkeeping() {
        let wrapped = bookkeeping_event(
            "Page.frameNavigated",
            br#"{"frame":{"id":"main","url":"file:///shell/index.html"}}"#,
        )
        .expect("a frame navigation is bookkeeping");
        let mut doc = DocumentGuard::default();
        assert!(doc.note(&wrapped));
        assert_eq!(
            doc.guard.main_url.as_deref(),
            Some("file:///shell/index.html")
        );
        assert!(!doc.gate.ready(), "bookkeeping names the URL, not a load");
    }

    #[test]
    fn a_context_created_round_trips_into_bookkeeping() {
        let mut doc = DocumentGuard::default();
        let wrapped = bookkeeping_event(
            "Page.frameNavigated",
            br#"{"frame":{"id":"main","url":"file:///shell/index.html"}}"#,
        )
        .expect("bookkeeping");
        assert!(doc.note(&wrapped));
        let wrapped = bookkeeping_event(
            "Runtime.executionContextCreated",
            br#"{"context":{"id":1,"auxData":{"isDefault":true,"frameId":"main"}}}"#,
        )
        .expect("a context announcement is bookkeeping");
        assert!(doc.note(&wrapped));
        assert!(doc.guard.context_allows(Some(1)));
        assert!(!doc.guard.context_allows(Some(2)));
    }

    #[test]
    fn contexts_cleared_round_trips_without_params() {
        let mut doc = DocumentGuard::default();
        let wrapped = bookkeeping_event(
            "Page.frameNavigated",
            br#"{"frame":{"id":"main","url":"file:///shell/index.html"}}"#,
        )
        .expect("bookkeeping");
        assert!(doc.note(&wrapped));
        let wrapped = bookkeeping_event(
            "Runtime.executionContextCreated",
            br#"{"context":{"id":1,"auxData":{"isDefault":true,"frameId":"main"}}}"#,
        )
        .expect("bookkeeping");
        assert!(doc.note(&wrapped));

        // Clearing carries no params at all.
        let wrapped =
            bookkeeping_event("Runtime.executionContextsCleared", b"").expect("bookkeeping");
        assert!(doc.note(&wrapped));
        assert!(!doc.guard.context_allows(Some(1)));
        assert!(!doc.gate.ready(), "the old document is going away");
    }

    #[test]
    fn malformed_params_are_not_noted() {
        assert!(bookkeeping_event("Page.frameNavigated", b"{not json").is_none());
        assert!(bookkeeping_event("Runtime.executionContextCreated", b"\xff").is_none());
    }

    #[test]
    fn other_methods_are_not_parsed() {
        // Not bookkeeping, so not even parsed — malformed params included.
        assert!(bookkeeping_event("Runtime.bindingCalled", b"{not json").is_none());
        assert!(bookkeeping_event("Page.loadEventFired", b"{not json").is_none());
        assert!(bookkeeping_event("Runtime.consoleAPICalled", b"{}").is_none());
    }

    #[test]
    fn a_frame_tree_result_round_trips_into_bookkeeping() {
        let wrapped = frame_tree_result(
            br#"{"frameTree":{"frame":{"id":"main","url":"file:///shell/index.html"}}}"#,
        )
        .expect("a frame tree is bookkeeping");
        let mut doc = DocumentGuard::default();
        assert!(doc.note(&wrapped));
        assert_eq!(
            doc.guard.main_url.as_deref(),
            Some("file:///shell/index.html")
        );
        assert!(!doc.gate.ready(), "the frame tree does not open the gate");
        assert!(doc.load_event_fired());
    }

    #[test]
    fn a_malformed_or_unrelated_result_is_not_noted() {
        assert!(frame_tree_result(b"{not json").is_none());
        // The reload's own result is not frame bookkeeping.
        assert!(frame_tree_result(b"{}").is_none());
    }
}
