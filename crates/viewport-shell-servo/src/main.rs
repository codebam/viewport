// SPDX-License-Identifier: MIT
//
// The shell, out of process, rendered by an embedded Servo.
//
// Servo was the original plan for this rewrite, and for a long time the entry
// in `docs/shell-backends.md` said "refused" for one reason: Servo is not a
// package. It is an rlib in a language with no stable ABI, so embedding it
// means compiling the engine, and nothing about that gets cheaper by being put
// off. This crate pays that bill exactly once and confines it:
//
// * It is outside the tree's workspace, with a lock file of its own. Nothing
//   the compositor builds, nothing the pre-commit hook runs and nothing CI
//   does reaches this manifest — `cargo test --workspace` cannot compile Servo
//   by accident, which is the failure mode worth designing against.
//
// * It links the engine into a shell *process*, not into the compositor.
//   `viewport-shell-servoshell` is the same engine without the build at all,
//   driven as a browser; that one is a workspace member because it links
//   nothing. Between them they are what `cef` and `chromium` are for Blink.
//
// What is here and what is Servo's:
//
// * **Pixels.** A winit window, a `WindowRenderingContext` over its raw
//   handles, and `WebView::paint` into it. The window is an ordinary Wayland
//   client on the connection the compositor made and handed over — winit takes
//   `WAYLAND_SOCKET` the same way libwayland does — so the desktop is drawn,
//   placed and paced exactly as the WebKitGTK backend's is. See
//   `crates/viewport/src/shell_client.rs`.
//
// * **Input.** winit events translated into `InputEvent`s. The keyboard
//   conversion here is deliberately small: the compositor owns every shortcut
//   and the shell's pages read `event.key`, not `event.code`, so characters
//   and a table of named keys are what is needed and `Code` stays
//   `Unidentified`. Servo's own port has the full 600-line table if that ever
//   stops being true (`ports/servoshell/desktop/keyutils.rs`).
//
// * **The bridge.** Servo has no `window.webkit.messageHandlers`, so both
//   directions are built from the web platform. Inbound is
//   `WebView::evaluate_javascript` delivering the same
//   `CustomEvent('viewport')` every other backend delivers. Outbound is a
//   `fetch` at a host that does not exist, intercepted by
//   `WebViewDelegate::load_web_resource` before it reaches the network — which
//   is a real embedder API rather than a console-message or dialog hack, and
//   costs the page nothing: `mode: 'no-cors'` needs no preflight and the reply
//   is discarded. `viewport_ipc::js::BRIDGE_SHIM` wraps the sender in the name
//   `data/shell/state.js` calls, so the shell's own JavaScript needs no edit.
//
// The one thing this backend does not do is show what is behind it:
// `WindowRenderingContext` asks surfman for the window's own configuration,
// which has no alpha, so a wallpaper terminal under the page would be
// invisible. `crates/viewport/src/shell_backend.rs` is where that is decided.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};

use anyhow::{anyhow, Context as _, Result};
use euclid::Scale;
use servo::{
    Code, DeviceIndependentPixel, DevicePixel, DevicePoint, InputEvent, Key, KeyState,
    KeyboardEvent, Location, Modifiers, MouseButton, MouseButtonAction, MouseButtonEvent,
    MouseMoveEvent, NamedKey, RenderingContext, Servo, ServoBuilder, UserContentManager,
    UserScript, WebResourceLoad, WebResourceResponse, WebView, WebViewBuilder, WebViewDelegate,
    WheelDelta, WheelEvent, WheelMode, WindowRenderingContext,
};
use url::Url;
use viewport_shell_bridge::{Line, Options};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WinitKey, KeyLocation, ModifiersState, NamedKey as WinitNamedKey};
use winit::platform::wayland::WindowAttributesExtWayland as _;
use winit::raw_window_handle::{HasDisplayHandle as _, HasWindowHandle as _};
use winit::window::Window;

/// The host the page's outbound messages are addressed to.
///
/// `.invalid` is reserved by RFC 2606 and resolves nowhere, which is the point:
/// if the interception below ever stops happening, the messages fail loudly
/// against a name that cannot be someone else's server.
const BRIDGE_HOST: &str = "viewport-ipc.invalid";

/// The `app_id` the window carries.
///
/// Not how the compositor identifies the shell — that is the connection it
/// made and handed to this process, which no other client can claim. This is
/// for a taskbar, a screen reader, a log.
const APP_ID: &str = "dev.viewport.shell";

/// A line's worth of winit scroll, in pixels. Servo's own port uses these.
const LINE_HEIGHT: f32 = 38.0;
const LINE_WIDTH: f32 = 38.0;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Before anything fetches. Servo leaves the choice of provider to the
    // embedder, and without one every https load fails at the handshake with
    // an error that does not mention rustls.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow!("a crypto provider was already installed"))?;

    let args: Vec<String> = std::env::args().collect();
    let options = Options::parse(&args)?;
    let url = Url::parse(&options.url).with_context(|| format!("{} is not a url", options.url))?;

    let event_loop = EventLoop::<Wake>::with_user_event()
        .build()
        .context("starting an event loop")?;
    let proxy = event_loop.create_proxy();

    // The compositor first, so that a page which loads immediately does not
    // send its first message into a socket nobody has opened.
    let (tx, lines) = mpsc::channel();
    let out = {
        let proxy = proxy.clone();
        viewport_shell_bridge::connect(&options.socket, move |line| {
            if tx.send(line).is_ok() {
                // The event loop is asleep until something wakes it, and a
                // layout event that arrives on a quiet desktop is exactly the
                // case that matters.
                let _ = proxy.send_event(Wake::Compositor);
            }
        })?
    };

    let mut app = App::Starting(Box::new(Starting {
        url,
        out,
        lines,
        waker: Waker(proxy),
        inspector: options.inspector,
    }));
    event_loop.run_app(&mut app).context("the event loop")?;
    Ok(())
}

/// Why the event loop was woken.
#[derive(Debug)]
enum Wake {
    /// Servo has work to do.
    Servo,
    /// The compositor said something.
    Compositor,
}

/// Servo's handle on the event loop.
#[derive(Clone)]
struct Waker(EventLoopProxy<Wake>);

impl servo::EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn servo::EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        if let Err(e) = self.0.send_event(Wake::Servo) {
            tracing::warn!("the event loop is gone: {e}");
        }
    }
}

/// Everything the shell needs before there is a window to hang it on.
struct Starting {
    url: Url,
    out: viewport_shell_bridge::Sender,
    lines: Receiver<Line>,
    waker: Waker,
    inspector: bool,
}

enum App {
    Starting(Box<Starting>),
    Running(Rc<Shell>),
}

/// The shell, once there is a window.
struct Shell {
    window: Window,
    servo: Servo,
    rendering_context: Rc<WindowRenderingContext>,
    /// Set from `resumed`, and never unset while the process lives.
    webview: RefCell<Option<WebView>>,
    out: viewport_shell_bridge::Sender,
    lines: Receiver<Line>,
    /// Where the pointer is, because winit reports a button press without one
    /// and Servo wants a point on every mouse event.
    pointer: Cell<DevicePoint>,
    /// Which modifiers are held, for the same reason.
    modifiers: Cell<ModifiersState>,
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let App::Starting(starting) = self else {
            // Wayland sends this once. A second one would mean rebuilding the
            // engine under a page that is already running.
            return;
        };

        match start(event_loop, starting) {
            Ok(shell) => *self = App::Running(shell),
            Err(e) => {
                tracing::error!("the shell could not start: {e:#}");
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Wake) {
        let App::Running(shell) = self else {
            return;
        };
        if matches!(event, Wake::Compositor) && !shell.drain_compositor() {
            tracing::info!("the compositor closed the socket; stopping");
            event_loop.exit();
            return;
        }
        shell.servo.spin_event_loop();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: winit::window::WindowId, event: WindowEvent) {
        let App::Running(shell) = self else {
            return;
        };
        shell.servo.spin_event_loop();

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => shell.present(),
            WindowEvent::Resized(size) => {
                // Both, and in this order: the context owns the surface Servo
                // draws into and the webview owns what it lays out at. A
                // compositor configure that reached only one of them is a
                // desktop laid out for the last size and painted at this one.
                shell.rendering_context.resize(size);
                if let Some(webview) = shell.webview.borrow().as_ref() {
                    webview.resize(size);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(webview) = shell.webview.borrow().as_ref() {
                    webview.set_hidpi_scale_factor(euclid_scale(scale_factor as f32));
                }
            }
            other => shell.input(other),
        }
    }
}

fn start(event_loop: &ActiveEventLoop, starting: &mut Starting) -> Result<Rc<Shell>> {
    let attributes = Window::default_attributes()
        .with_title("Viewport")
        // Wayland's `app_id`, which winit calls the general name.
        .with_name(APP_ID, "");
    let window = event_loop
        .create_window(attributes)
        .context("creating the shell window")?;

    let rendering_context = Rc::new(
        WindowRenderingContext::new(
            event_loop.display_handle().context("the display handle")?,
            window.window_handle().context("the window handle")?,
            window.inner_size(),
        )
        .map_err(|e| anyhow!("no rendering context for the shell window: {e:?}"))?,
    );
    rendering_context
        .make_current()
        .map_err(|e| anyhow!("making the shell's context current: {e:?}"))?;

    let servo = ServoBuilder::default()
        .event_loop_waker(Box::new(starting.waker.clone()))
        .build();
    servo.setup_logging();

    // Injected before the page's own scripts, which is what makes the bridge
    // exist by the time `data/shell/state.js` looks for it.
    let user_content = Rc::new(UserContentManager::new(&servo));
    user_content.add_script(Rc::new(UserScript::new(bridge_script(), None)));

    let shell = Rc::new(Shell {
        window,
        servo,
        rendering_context: rendering_context.clone(),
        webview: RefCell::new(None),
        out: starting.out.clone(),
        lines: std::mem::replace(&mut starting.lines, mpsc::channel().1),
        pointer: Cell::new(DevicePoint::new(0.0, 0.0)),
        modifiers: Cell::new(ModifiersState::empty()),
    });

    let webview = WebViewBuilder::new(&shell.servo, rendering_context)
        .url(starting.url.clone())
        .hidpi_scale_factor(euclid_scale(shell.window.scale_factor() as f32))
        .user_content_manager(user_content)
        .delegate(shell.clone())
        .build();
    webview.focus();
    webview.show();
    *shell.webview.borrow_mut() = Some(webview);

    if starting.inspector {
        // Servo's inspector is its devtools server, which is a port rather
        // than a pipe and so is only ever opened when asked for.
        tracing::info!("--inspector: start servo with `--devtools` to attach; this build has no in-process inspector");
    }

    tracing::info!("shell: servo is up on {}", starting.url);
    Ok(shell)
}

impl Shell {
    /// Paint the page and put it on the screen.
    fn present(&self) {
        if let Some(webview) = self.webview.borrow().as_ref() {
            webview.paint();
            self.rendering_context.present();
        }
    }

    /// Deliver everything the compositor has said. False once it has stopped.
    fn drain_compositor(&self) -> bool {
        while let Ok(line) = self.lines.try_recv() {
            match line {
                Line::Closed => return false,
                Line::Event(json) => {
                    let Some(webview) = self.webview.borrow().clone() else {
                        continue;
                    };
                    if viewport_shell_bridge::is_reload(&json) {
                        tracing::info!("reloading the shell");
                        webview.reload();
                        continue;
                    }
                    // The same script every other backend evaluates, built by
                    // the same function, so a message that reaches the page
                    // here is the message that reaches it there.
                    webview.evaluate_javascript(viewport_ipc::js::dispatch(&json), |result| {
                        if let Err(e) = result {
                            tracing::warn!("the page refused an event: {e:?}");
                        }
                    });
                }
            }
        }
        true
    }

    /// One winit event, as far as Servo is concerned.
    fn input(&self, event: WindowEvent) {
        let Some(webview) = self.webview.borrow().clone() else {
            return;
        };
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let point = DevicePoint::new(position.x as f32, position.y as f32);
                self.pointer.set(point);
                webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(point.into())));
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let action = match state {
                    ElementState::Pressed => MouseButtonAction::Down,
                    ElementState::Released => MouseButtonAction::Up,
                };
                webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                    action,
                    mouse_button(button),
                    self.pointer.get().into(),
                )));
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (x, y) = match delta {
                    // Reported in pixels either way: a line is a unit only the
                    // toolkit knows the size of, and Servo scrolls by pixels.
                    MouseScrollDelta::LineDelta(x, y) => {
                        ((x * LINE_WIDTH) as f64, (y * LINE_HEIGHT) as f64)
                    }
                    MouseScrollDelta::PixelDelta(delta) => (delta.x, delta.y),
                };
                webview.notify_input_event(InputEvent::Wheel(WheelEvent::new(
                    WheelDelta {
                        x,
                        y,
                        z: 0.0,
                        mode: WheelMode::DeltaPixel,
                    },
                    self.pointer.get().into(),
                )));
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers.set(modifiers.state()),
            WindowEvent::KeyboardInput { event, .. } => {
                webview.notify_input_event(InputEvent::Keyboard(keyboard_event(
                    &event,
                    self.modifiers.get(),
                )));
            }
            _ => {}
        }
    }
}

impl WebViewDelegate for Shell {
    fn notify_new_frame_ready(&self, _webview: WebView) {
        self.window.request_redraw();
    }

    /// The outbound half of the bridge.
    ///
    /// Every resource the page loads passes through here, so the first thing
    /// this does is decide the request is not ours and say nothing — a load
    /// that is not intercepted continues as normal.
    fn load_web_resource(&self, _webview: WebView, load: WebResourceLoad) {
        let url = load.request().url.clone();
        if url.host_str() != Some(BRIDGE_HOST) {
            return;
        }
        match url.query_pairs().find(|(key, _)| key == "m") {
            Some((_, message)) => self.out.send(message.into_owned()),
            None => tracing::warn!("a bridge request carried no message: {url}"),
        }
        // Answered rather than cancelled: a cancelled fetch rejects, and the
        // page would log an error for every message it successfully sent.
        load.intercept(WebResourceResponse::new(url)).finish();
    }

    fn show_console_message(&self, _webview: WebView, _level: servo::ConsoleLogLevel, message: String) {
        // The shell's own diagnostics, in the compositor's log, where
        // `~/viewport.log` already has everything else about this session.
        tracing::info!("shell console: {message}");
    }

    fn notify_crashed(&self, _webview: WebView, reason: String, backtrace: Option<String>) {
        tracing::error!("the shell's engine crashed: {reason}");
        if let Some(backtrace) = backtrace {
            tracing::error!("{backtrace}");
        }
        // Exit rather than sit in front of a dead page. The compositor
        // restarts the shell five times in a minute and then leaves it down,
        // which is the same policy every out-of-process backend gets.
        std::process::exit(1);
    }
}

/// The script that puts the compositor within reach of the page.
///
/// `__viewport_send` first, because `BRIDGE_SHIM` looks for it and gives up
/// loudly if it is not there.
fn bridge_script() -> String {
    format!(
        r#"(function () {{
  'use strict';
  window.__viewport_send = function (message) {{
    /* Intercepted by the embedder before it reaches the network. `no-cors`
       so that no preflight is needed and no origin matters; the response is
       an empty 200 nobody reads. */
    fetch('http://{BRIDGE_HOST}/send?m=' + encodeURIComponent(message), {{
      mode: 'no-cors',
      cache: 'no-store',
      keepalive: true,
    }}).catch(function (e) {{
      console.error('viewport: the compositor bridge refused a message', e);
    }});
  }};
}})();
{shim}
"#,
        shim = viewport_ipc::js::BRIDGE_SHIM
    )
}

fn mouse_button(button: winit::event::MouseButton) -> MouseButton {
    match button {
        winit::event::MouseButton::Left => MouseButton::Left,
        winit::event::MouseButton::Right => MouseButton::Right,
        winit::event::MouseButton::Middle => MouseButton::Middle,
        winit::event::MouseButton::Back => MouseButton::Back,
        winit::event::MouseButton::Forward => MouseButton::Forward,
        winit::event::MouseButton::Other(other) => MouseButton::Other(other),
    }
}

fn euclid_scale(factor: f32) -> Scale<f32, DeviceIndependentPixel, DevicePixel> {
    Scale::new(factor)
}

/// winit's idea of a key press, as `keyboard_types`'.
///
/// `Code` is left `Unidentified` on purpose: it is the physical key, the shell
/// does not read it, and filling it in is the 600-line table in Servo's own
/// port. See the header.
fn keyboard_event(event: &winit::event::KeyEvent, modifiers: ModifiersState) -> KeyboardEvent {
    let key = match &event.logical_key {
        WinitKey::Character(text) => Key::Character(text.to_string()),
        WinitKey::Named(WinitNamedKey::Space) => Key::Character(" ".to_owned()),
        WinitKey::Named(named) => Key::Named(named_key(*named)),
        WinitKey::Dead(_) | WinitKey::Unidentified(_) => Key::Named(NamedKey::Unidentified),
    };
    let state = match event.state {
        ElementState::Pressed => KeyState::Down,
        ElementState::Released => KeyState::Up,
    };
    let location = match event.location {
        KeyLocation::Standard => Location::Standard,
        KeyLocation::Left => Location::Left,
        KeyLocation::Right => Location::Right,
        KeyLocation::Numpad => Location::Numpad,
    };

    let mut held = Modifiers::empty();
    held.set(Modifiers::CONTROL, modifiers.control_key());
    held.set(Modifiers::SHIFT, modifiers.shift_key());
    held.set(Modifiers::ALT, modifiers.alt_key());
    held.set(Modifiers::META, modifiers.super_key());

    KeyboardEvent::new_without_event(
        state,
        key,
        Code::Unidentified,
        location,
        held,
        event.repeat,
        false,
    )
}

/// The named keys a page can act on. Everything else is `Unidentified`.
fn named_key(named: WinitNamedKey) -> NamedKey {
    match named {
        WinitNamedKey::Enter => NamedKey::Enter,
        WinitNamedKey::Tab => NamedKey::Tab,
        WinitNamedKey::Escape => NamedKey::Escape,
        WinitNamedKey::Backspace => NamedKey::Backspace,
        WinitNamedKey::Delete => NamedKey::Delete,
        WinitNamedKey::Insert => NamedKey::Insert,
        WinitNamedKey::Home => NamedKey::Home,
        WinitNamedKey::End => NamedKey::End,
        WinitNamedKey::PageUp => NamedKey::PageUp,
        WinitNamedKey::PageDown => NamedKey::PageDown,
        WinitNamedKey::ArrowUp => NamedKey::ArrowUp,
        WinitNamedKey::ArrowDown => NamedKey::ArrowDown,
        WinitNamedKey::ArrowLeft => NamedKey::ArrowLeft,
        WinitNamedKey::ArrowRight => NamedKey::ArrowRight,
        WinitNamedKey::Shift => NamedKey::Shift,
        WinitNamedKey::Control => NamedKey::Control,
        WinitNamedKey::Alt => NamedKey::Alt,
        WinitNamedKey::Super => NamedKey::Meta,
        WinitNamedKey::CapsLock => NamedKey::CapsLock,
        WinitNamedKey::F1 => NamedKey::F1,
        WinitNamedKey::F2 => NamedKey::F2,
        WinitNamedKey::F3 => NamedKey::F3,
        WinitNamedKey::F4 => NamedKey::F4,
        WinitNamedKey::F5 => NamedKey::F5,
        WinitNamedKey::F6 => NamedKey::F6,
        WinitNamedKey::F7 => NamedKey::F7,
        WinitNamedKey::F8 => NamedKey::F8,
        WinitNamedKey::F9 => NamedKey::F9,
        WinitNamedKey::F10 => NamedKey::F10,
        WinitNamedKey::F11 => NamedKey::F11,
        WinitNamedKey::F12 => NamedKey::F12,
        _ => NamedKey::Unidentified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The page is written against WebKit's name for the bridge and this
    /// engine has none, so the shim is the only thing that makes `postMessage`
    /// exist — and it has to come after the sender it wraps.
    #[test]
    fn the_script_defines_the_sender_before_it_is_wrapped() {
        let script = bridge_script();
        let sender = script
            .find("window.__viewport_send")
            .expect("the sender is defined");
        let shim = script
            .find("window.webkit.messageHandlers.viewport = handler")
            .expect("the shim installs the name");
        assert!(sender < shim, "{script}");
    }

    /// The interception in `load_web_resource` matches on this host, so a
    /// script addressing a different one is a shell that silently says
    /// nothing.
    #[test]
    fn the_script_addresses_the_host_the_delegate_intercepts() {
        assert!(bridge_script().contains(&format!("http://{BRIDGE_HOST}/send?m=")));
    }
}
