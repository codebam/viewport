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

use std::ffi::{c_char, CString};

use anyhow::{anyhow, Result};
use cef::{args::Args, rc::*, *};

use viewport_shell_bridge::Options;

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

    let settings = Settings {
        // No sandbox: the helper is a setuid binary and a nix store path
        // cannot be one. The shell is a page this compositor shipped rather
        // than the open web.
        no_sandbox: 1,
        ..Default::default()
    };

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
    //                             path cannot be one.
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

    tracing::info!("CEF is up on {}", options.url);
    run_message_loop();
    shutdown();
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
            let Some(view) = browser_view_create(
                None,
                Some(&CefString::from(self.url.as_str())),
                Some(&BrowserSettings::default()),
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
            window.add_child_view(Some(&mut child));
            window.show();
        }

        /// No titlebar and no border: this window is the desktop, and what
        /// goes around a window is drawn by the compositor.
        fn is_frameless(&self, _window: Option<&mut Window>) -> ::std::os::raw::c_int {
            1
        }
    }
}
