// SPDX-License-Identifier: MIT
//
// The WebKit web view, in Rust.
//
// The C shim exists for GObject *subclassing* — filling a class struct's vfunc
// pointers, where the compiler checks the assignments. Creating an object and
// connecting a signal are not that, so they are here.
//
// No variadic FFI is involved, which is the thing that would have made this
// unpleasant. `g_object_new` is varargs, but `g_object_new_with_properties`
// takes arrays; `g_signal_connect` is a macro over `g_signal_connect_data`,
// which is an ordinary function. Both of those are what this uses.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::Path;

use anyhow::{anyhow, Result};

use crate::wpe::Display;

type GType = usize;
type GBool = i32;

/// `GValue` is `{ GType; union[2] }` — 24 bytes on a 64-bit target.
///
/// Declared rather than bound because its layout is part of GObject's public
/// ABI and has been stable for the life of the library. `g_value_init` is what
/// actually initialises it; zeroing first is required.
#[repr(C)]
#[derive(Clone, Copy)]
struct GValue {
    g_type: GType,
    data: [u64; 2],
}

impl GValue {
    fn zeroed() -> Self {
        Self {
            g_type: 0,
            data: [0; 2],
        }
    }
}

#[repr(C)]
struct GError {
    domain: u32,
    code: i32,
    message: *mut c_char,
}

extern "C" {
    // GObject
    fn g_object_new_with_properties(
        object_type: GType,
        n_properties: u32,
        names: *const *const c_char,
        values: *const GValue,
    ) -> *mut c_void;
    fn g_object_unref(object: *mut c_void);
    fn g_object_get_type() -> GType;
    fn g_value_init(value: *mut GValue, g_type: GType) -> *mut GValue;
    fn g_value_set_object(value: *mut GValue, object: *mut c_void);
    fn g_value_unset(value: *mut GValue);
    fn g_signal_connect_data(
        instance: *mut c_void,
        detailed_signal: *const c_char,
        handler: Option<unsafe extern "C" fn()>,
        data: *mut c_void,
        destroy: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
        flags: u32,
    ) -> u64;
    fn g_error_free(error: *mut GError);
    fn g_free(pointer: *mut c_void);

    // WPE
    fn wpe_display_connect(display: *mut c_void, error: *mut *mut GError) -> GBool;

    // WebKit
    fn webkit_web_view_get_type() -> GType;
    fn webkit_user_content_manager_new() -> *mut c_void;
    fn webkit_user_content_manager_register_script_message_handler(
        manager: *mut c_void,
        name: *const c_char,
        world_name: *const c_char,
    ) -> GBool;
    fn webkit_web_view_load_uri(view: *mut c_void, uri: *const c_char);
    fn webkit_web_view_reload_bypass_cache(view: *mut c_void);
    fn webkit_web_view_get_uri(view: *mut c_void) -> *const c_char;
    fn webkit_web_view_get_wpe_view(view: *mut c_void) -> *mut c_void;
    fn wpe_view_buffer_released(view: *mut c_void, buffer: *mut c_void);
    fn webkit_web_view_get_settings(view: *mut c_void) -> *mut c_void;
    fn webkit_settings_set_enable_write_console_messages_to_stdout(
        settings: *mut c_void,
        enabled: GBool,
    );
    fn webkit_web_view_evaluate_javascript(
        view: *mut c_void,
        script: *const c_char,
        length: isize,
        world_name: *const c_char,
        source_uri: *const c_char,
        cancellable: *mut c_void,
        callback: Option<unsafe extern "C" fn()>,
        user_data: *mut c_void,
    );

    // JavaScriptCore
    fn jsc_value_is_string(value: *mut c_void) -> GBool;
    fn jsc_value_to_string(value: *mut c_void) -> *mut c_char;
    fn jsc_value_to_json(value: *mut c_void, indent: u32) -> *mut c_char;
}

/// What the page sends back.
pub trait MessageSink: Send {
    /// One `postMessage` from the shell, as JSON.
    fn message(&mut self, json: &str);
}

/// Why WebKit's web process went away.
///
/// The values are `WebKitWebProcessTerminationReason`, which is part of the
/// library's public ABI; anything unrecognised is treated as a crash, because
/// the recovery is the same and refusing to act on an enum variant added in a
/// later WebKit would leave the desktop dead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    Crashed,
    ExceededMemoryLimit,
    TerminatedByApi,
}

impl Termination {
    fn from_raw(reason: u32) -> Self {
        match reason {
            1 => Self::ExceededMemoryLimit,
            2 => Self::TerminatedByApi,
            _ => Self::Crashed,
        }
    }

    /// Whether the compositor should try to bring the shell back.
    ///
    /// A crash or an OOM kill is something to recover from. A termination
    /// asked for through the API is not: something wanted the process gone,
    /// and reloading would fight it.
    pub fn is_recoverable(self) -> bool {
        !matches!(self, Self::TerminatedByApi)
    }
}

impl std::fmt::Display for Termination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Crashed => "the web process crashed",
            Self::ExceededMemoryLimit => "the web process exceeded its memory limit",
            Self::TerminatedByApi => "the web process was terminated by an API call",
        })
    }
}

/// Told when WebKit's web process dies.
///
/// The web process is not the compositor's process, so its death is survivable
/// — but nothing recovers on its own. WebKit leaves the view blank and stops
/// painting, which on a desktop whose entire UI is that view is
/// indistinguishable from a compositor that has hung.
pub trait CrashSink: Send {
    fn terminated(&mut self, reason: Termination);
}

/// A WebKit web view rendering into our WPE display.
pub struct WebView {
    view: *mut c_void,
    manager: *mut c_void,
    // Kept alive as long as the signal handler can fire.
    _sink: Box<Box<dyn MessageSink>>,
    _crash: Box<Box<dyn CrashSink>>,
    // The display must outlive the view.
    //
    // `Rc` and not `Arc`: `Display` wraps a raw WPE pointer and is neither
    // `Send` nor `Sync`, and everything that touches it runs on the thread
    // holding the GLib main context. An `Arc` would only pay for atomics it
    // cannot make sound anyway.
    _display: std::rc::Rc<Display>,
}

impl WebView {
    /// Connect the display and create a view on it.
    ///
    /// `console` mirrors the page's console into stdout. A shell that throws
    /// during startup otherwise renders nothing and says nothing, and the
    /// compositor just shows an empty desktop.
    pub fn new(
        display: std::rc::Rc<Display>,
        sink: Box<dyn MessageSink>,
        crash: Box<dyn CrashSink>,
        console: bool,
    ) -> Result<Self> {
        // SAFETY: the display handle is valid for the lifetime of `display`,
        // which this holds.
        let mut error: *mut GError = std::ptr::null_mut();
        let connected = unsafe { wpe_display_connect(display.handle(), &mut error) };
        if connected == 0 {
            // SAFETY: the error is ours if the call failed, and is freed here.
            let message = unsafe { take_error(error, "wpe_display_connect failed") };
            return Err(anyhow!(message));
        }

        let mut sink = Box::new(sink);
        let user = &mut *sink as *mut Box<dyn MessageSink> as *mut c_void;

        let mut crash = Box::new(crash);
        let crash_user = &mut *crash as *mut Box<dyn CrashSink> as *mut c_void;

        // SAFETY: every call below is a plain GObject function on objects
        // this owns, with arguments that outlive the call.
        unsafe {
            let manager = webkit_user_content_manager_new();
            if manager.is_null() {
                return Err(anyhow!("webkit_user_content_manager_new returned NULL"));
            }

            // The name the shell reaches for:
            // window.webkit.messageHandlers.viewport.postMessage(...)
            let name = CString::new("viewport").unwrap();
            if webkit_user_content_manager_register_script_message_handler(
                manager,
                name.as_ptr(),
                std::ptr::null(),
            ) == 0
            {
                g_object_unref(manager);
                return Err(anyhow!("could not register the script message handler"));
            }

            // Detailed signal: only messages for our handler name.
            let signal = CString::new("script-message-received::viewport").unwrap();
            g_signal_connect_data(
                manager,
                signal.as_ptr(),
                Some(std::mem::transmute::<
                    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void),
                    unsafe extern "C" fn(),
                >(on_script_message)),
                user,
                None,
                0,
            );

            // Constructed with properties rather than g_object_new's varargs.
            let display_property = CString::new("display").unwrap();
            let manager_property = CString::new("user-content-manager").unwrap();

            let mut display_value = GValue::zeroed();
            let mut manager_value = GValue::zeroed();
            g_value_init(&mut display_value, g_object_get_type());
            g_value_init(&mut manager_value, g_object_get_type());
            g_value_set_object(&mut display_value, display.handle());
            g_value_set_object(&mut manager_value, manager);

            let names = [display_property.as_ptr(), manager_property.as_ptr()];
            let values = [display_value, manager_value];
            let view = g_object_new_with_properties(
                webkit_web_view_get_type(),
                2,
                names.as_ptr(),
                values.as_ptr(),
            );

            g_value_unset(&mut display_value);
            g_value_unset(&mut manager_value);

            if view.is_null() {
                g_object_unref(manager);
                return Err(anyhow!("the web view could not be created"));
            }

            // Connected on the view rather than the content manager, because
            // this is the view's signal — and only once the view exists, which
            // is why it is here and not up with the message handler.
            let terminated = CString::new("web-process-terminated").unwrap();
            g_signal_connect_data(
                view,
                terminated.as_ptr(),
                Some(std::mem::transmute::<
                    unsafe extern "C" fn(*mut c_void, u32, *mut c_void),
                    unsafe extern "C" fn(),
                >(on_web_process_terminated)),
                crash_user,
                None,
                0,
            );

            if console {
                let settings = webkit_web_view_get_settings(view);
                if !settings.is_null() {
                    webkit_settings_set_enable_write_console_messages_to_stdout(settings, 1);
                }
            }

            Ok(Self {
                view,
                manager,
                _sink: sink,
                _crash: crash,
                _display: display,
            })
        }
    }

    /// Load a URL, replacing whatever is showing.
    pub fn load(&self, uri: &str) -> Result<()> {
        let uri = CString::new(uri).map_err(|_| anyhow!("the URL contains a nul byte"))?;
        // SAFETY: `view` is valid and the string outlives the call.
        unsafe { webkit_web_view_load_uri(self.view, uri.as_ptr()) };
        Ok(())
    }

    /// Load a file path as a `file://` URL.
    pub fn load_file(&self, path: &Path) -> Result<()> {
        self.load(&format!("file://{}", path.display()))
    }

    /// Deliver a message to the page.
    ///
    /// Becomes the same `CustomEvent` the C build dispatches, so the shell's
    /// existing listener needs no change.
    pub fn post(&self, json: &str) -> Result<()> {
        let script = format!(
            "window.dispatchEvent(new CustomEvent('viewport',{{detail:JSON.parse({})}}));",
            js_string_literal(json)
        );
        self.evaluate(&script)
    }

    /// Run a script in the page.
    pub fn evaluate(&self, script: &str) -> Result<()> {
        let script = CString::new(script).map_err(|_| anyhow!("the script contains a nul byte"))?;
        // SAFETY: -1 means "nul-terminated"; the remaining pointers are
        // optional and null here, and no callback is registered because
        // nothing needs the result.
        unsafe {
            webkit_web_view_evaluate_javascript(
                self.view,
                script.as_ptr(),
                -1,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                None,
                std::ptr::null_mut(),
            );
        }
        Ok(())
    }

    /// What the view is showing, if anything.
    ///
    /// Needed after a crash: reloading is not enough on its own, because a web
    /// process that died mid-load leaves the view with nothing to reload.
    pub fn uri(&self) -> Option<String> {
        // SAFETY: `view` is valid. The string is WebKit's and stays valid
        // until the view navigates, which cannot happen across this copy.
        unsafe {
            let uri = webkit_web_view_get_uri(self.view);
            if uri.is_null() {
                return None;
            }
            Some(CStr::from_ptr(uri).to_string_lossy().into_owned())
        }
    }

    /// Reload, ignoring the HTTP cache. The escape hatch for a shell being
    /// edited live.
    pub fn reload(&self) {
        // SAFETY: `view` is valid.
        unsafe { webkit_web_view_reload_bypass_cache(self.view) };
    }

    /// Give a frame's buffer back to WebKit's pool.
    ///
    /// Distinct from acknowledging it, which the shim's `frame_done` does:
    /// acknowledging says the frame reached the screen and lets the frame
    /// clock schedule the next paint, this says nothing samples the memory
    /// any more. A compositor that only ever acknowledges never returns a
    /// buffer, so the pool drains and WebKit stops painting — with the last
    /// frame still on screen, which reads as a frozen display rather than a
    /// stalled engine.
    ///
    /// Goes through `webkit_web_view_get_wpe_view` rather than the shim
    /// because the shim is only there for the GObject subclassing that cannot
    /// be done safely from Rust, and this is a plain call.
    pub fn frame_release(&self, token: &crate::wpe::FrameToken) {
        // SAFETY: `view` is valid, and the buffer came from this view's own
        // render_buffer. Both calls are on the thread that drives GLib.
        unsafe {
            let wpe_view = webkit_web_view_get_wpe_view(self.view);
            if wpe_view.is_null() {
                // Nothing would ever get its buffers back, so say so rather
                // than silently stalling the engine one frame later.
                tracing::error!("the web view has no WPEView to release against");
                return;
            }
            wpe_view_buffer_released(wpe_view, token.as_ptr());
        }
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        // SAFETY: both were referenced on construction and are dropped once.
        unsafe {
            g_object_unref(self.view);
            g_object_unref(self.manager);
        }
    }
}

impl std::fmt::Debug for WebView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebView").finish_non_exhaustive()
    }
}

/// The `script-message-received` handler.
unsafe extern "C" fn on_script_message(
    _manager: *mut c_void,
    value: *mut c_void,
    user: *mut c_void,
) {
    if user.is_null() || value.is_null() {
        return;
    }

    // A panic must not unwind into C.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Accept either a JSON string or a live object, so page authors can
        // call postMessage({...}) without stringifying by hand — same as the
        // C build.
        let text = if jsc_value_is_string(value) != 0 {
            jsc_value_to_string(value)
        } else {
            jsc_value_to_json(value, 0)
        };
        if text.is_null() {
            return;
        }

        if let Ok(json) = CStr::from_ptr(text).to_str() {
            let sink = &mut *(user as *mut Box<dyn MessageSink>);
            sink.message(json);
        }
        g_free(text as *mut c_void);
    }));
}

/// The `web-process-terminated` handler.
unsafe extern "C" fn on_web_process_terminated(_view: *mut c_void, reason: u32, user: *mut c_void) {
    if user.is_null() {
        return;
    }

    // A panic must not unwind into C — and this one runs while WebKit is
    // already handling a death, which is the worst place to add a second.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let sink = &mut *(user as *mut Box<dyn CrashSink>);
        sink.terminated(Termination::from_raw(reason));
    }));
}

/// Take a `GError`'s message and free it.
unsafe fn take_error(error: *mut GError, fallback: &str) -> String {
    if error.is_null() {
        return fallback.to_owned();
    }
    let message = if (*error).message.is_null() {
        fallback.to_owned()
    } else {
        CStr::from_ptr((*error).message)
            .to_string_lossy()
            .into_owned()
    };
    g_error_free(error);
    message
}

/// Quote a string as a JavaScript literal.
///
/// The message is interpolated into a script, so anything that could end the
/// literal early has to be escaped — a shell message containing a quote would
/// otherwise be a syntax error at best.
fn js_string_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // U+2028 and U+2029 terminate a line in JavaScript but not in
            // JSON, so a message containing one would end the statement.
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quote_cannot_end_the_literal_early() {
        // The message is interpolated into a script, so this is the difference
        // between a delivered message and a syntax error.
        assert_eq!(js_string_literal(r#"a"b"#), r#""a\"b""#);
        assert_eq!(js_string_literal(r"a\b"), r#""a\\b""#);
    }

    #[test]
    fn newlines_are_escaped() {
        assert_eq!(js_string_literal("a\nb"), r#""a\nb""#);
        assert_eq!(js_string_literal("a\r\nb"), r#""a\r\nb""#);
    }

    #[test]
    fn the_javascript_only_line_terminators_are_escaped() {
        // Legal inside a JSON string, but they end a line in JavaScript — so
        // interpolating one raw truncates the statement.
        assert_eq!(js_string_literal("a\u{2028}b"), r#""a\u2028b""#);
        assert_eq!(js_string_literal("a\u{2029}b"), r#""a\u2029b""#);
    }

    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(js_string_literal("a\u{1}b"), r#""a\u0001b""#);
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        assert_eq!(
            js_string_literal(r#"{"type":"view.layout","id":1}"#),
            r#""{\"type\":\"view.layout\",\"id\":1}""#
        );
    }
}
