// SPDX-License-Identifier: MIT
//
// The page-facing half of the protocol.
//
// Inbound messages do not reach the shell as a function call. They reach it as
// a script the engine evaluates, which dispatches the `CustomEvent` that
// `data/shell/state.js` listens for — so the exact text of that script is part
// of the contract, not an implementation detail of one engine.
//
// It lives here because there is now more than one thing that has to produce
// it: the in-process WPE backend evaluates it through
// `webkit_web_view_evaluate_javascript`, and the out-of-process WebKitGTK
// shell evaluates it through the GObject binding for the same call. A message
// the two deliver differently is a shell that behaves differently depending on
// which engine is installed, which is the one thing a second backend must not
// introduce.

/// The environment variable that opts a session into loading its shell from
/// an origin that is not a local file: or loopback.
///
/// The page-to-compositor bridge is privileged — a message from it can run a
/// command — so a remote page is only trusted when somebody deliberately said
/// so for this run.
pub const ALLOW_REMOTE_SHELL_ENV: &str = "VIEWPORT_ALLOW_REMOTE_SHELL";

/// Whether this process was told it may load its shell from a remote origin.
pub fn remote_shell_allowed() -> bool {
    matches!(std::env::var(ALLOW_REMOTE_SHELL_ENV), Ok(value) if value == "1")
}

/// Whether the bridge may speak for a document at `url`.
///
/// Allowed by default: `file:` and loopback `http(s)` — `localhost`,
/// `127.0.0.1` and `[::1]`, with or without a port. Every other origin is
/// allowed only when [`ALLOW_REMOTE_SHELL_ENV`] is `1`, because a message
/// from this bridge can run a command as the user.
///
/// This is deliberately a string classifier rather than URL parsing: it is
/// small, has no dependencies, and is shared by the compositor's startup
/// check and every shell backend's runtime check.
pub fn shell_url_allowed(url: &str) -> bool {
    remote_shell_allowed() || is_local_shell_url(url)
}

/// The same origin policy without the environment override, for the tests.
fn is_local_shell_url(url: &str) -> bool {
    let url = url.trim();
    if url.is_empty() {
        return false;
    }
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    match scheme.to_ascii_lowercase().as_str() {
        "file" => {
            // `file:///path` and `file:/path` are local. A `file://host/path`
            // with a real host is a UNC-style remote path on some platforms,
            // so only an empty or loopback authority is allowed.
            let rest = rest.strip_prefix("//").unwrap_or(rest);
            let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
            if authority.is_empty() {
                // `file:///path` leaves one leading slash; `file:////host/share`
                // leaves two, which is a UNC path on the platforms that have
                // them. An empty authority must not be used to smuggle that
                // form past the host check.
                return !rest.starts_with("//");
            }
            authority_is_loopback(authority)
        }
        "http" | "https" => {
            // `http:/foo` is not a URL a browser will load; refusing it here
            // costs nothing and keeps the authority parser from seeing junk.
            let rest = rest.strip_prefix("//").unwrap_or("");
            let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
            authority_is_loopback(authority)
        }
        _ => false,
    }
}

/// Whether an authority component is exactly a loopback host.
///
/// Userinfo is refused outright: `http://localhost@evil.example/` has an
/// origin of `evil.example`, and a classifier that looked for `localhost`
/// anywhere before the path would let it through.
fn authority_is_loopback(authority: &str) -> bool {
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((host, tail)) = bracketed.split_once(']') else {
            return false;
        };
        if !host.eq_ignore_ascii_case("::1") {
            return false;
        }
        return tail.is_empty() || tail.strip_prefix(':').is_some_and(valid_port);
    }
    match authority.split_once(':') {
        Some((host, port)) => loopback_host(host) && valid_port(port),
        None => loopback_host(authority),
    }
}

fn loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1"
}

fn valid_port(port: &str) -> bool {
    !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
}

/// Quote a string as a JavaScript literal.
///
/// The message is interpolated into a script, so anything that could end the
/// literal early has to be escaped — a shell message containing a quote would
/// otherwise be a syntax error at best.
pub fn string_literal(text: &str) -> String {
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

/// The script that delivers one already-serialised event to the page.
///
/// `JSON.parse` of a quoted literal rather than the JSON inlined as an object
/// expression: the message is data, and a page that receives it must not be
/// able to have it evaluated as code.
pub fn dispatch(json: &str) -> String {
    format!(
        "window.dispatchEvent(new CustomEvent('viewport',{{detail:JSON.parse({})}}));",
        string_literal(json)
    )
}

/// The script injected before any of the shell's own scripts.
///
/// Servo has no `window.webkit.messageHandlers`, which is the entire outbound
/// half of the shell's bridge (`data/shell/state.js:13`). Rather than edit the
/// shell — the one thing this rewrite is supposed to carry over untouched — the
/// bridge is recreated in the page under the name the shell already looks for.
///
/// The inbound half needs nothing: `src/web.c:48` already delivers messages as
/// a `CustomEvent`, which is a plain DOM API Servo has.
///
/// `__viewport_send` is whatever primitive the Servo embedding gives us for
/// page-to-embedder messages; it is installed by the engine before this runs.
pub const BRIDGE_SHIM: &str = r#"
(function () {
  'use strict';
  /* This bridge is a privileged channel: a message from it can run a command.
   * It is for the shell document itself, never for a frame inside it, so it
   * is not installed where it cannot be trusted. */
  if (window.top !== window) {
    return;
  }
  if (window.webkit && window.webkit.messageHandlers &&
      window.webkit.messageHandlers.viewport) {
    return;
  }
  const send = window.__viewport_send;
  if (typeof send !== 'function') {
    console.error('viewport: no host bridge; the shell will not be able to lay anything out');
    return;
  }
  const handler = {
    /* The compositor accepts either a JSON string or a live object, so page
     * authors can call postMessage({...}) without stringifying by hand
     * (src/web.c:63). Preserve that. */
    postMessage(message) {
      send(typeof message === 'string' ? message : JSON.stringify(message));
    },
  };
  window.webkit = window.webkit || {};
  window.webkit.messageHandlers = window.webkit.messageHandlers || {};
  window.webkit.messageHandlers.viewport = handler;
})();
"#;

/// A script injected at document start into every frame, before the page's
/// own scripts.
///
/// WebKit creates `window.webkit.messageHandlers.viewport` in subframes too,
/// and the native `script-message-received` signal carries no frame identity,
/// so a handler installed by a page inside an iframe could reach the
/// compositor. The top document may keep the real handler; every subframe has
/// it replaced with a no-op before that frame's scripts run.
pub const SUBFRAME_GUARD: &str = r#"
(function () {
  'use strict';
  if (window.top === window) {
    return;
  }
  try {
    var handlers = window.webkit && window.webkit.messageHandlers;
    if (!handlers || !handlers.viewport) {
      return;
    }
    var disabled = {
      postMessage: function () {
        console.error('viewport: the page bridge is top-frame only; this is a subframe');
      },
    };
    /* Replace the whole handler where the property allows it, and fall back
     * to replacing the method on the handler object. WebKit creates these as
     * ordinary JS properties, but a frame that cannot be guarded must still
     * not stop the document from running. */
    try {
      Object.defineProperty(handlers, 'viewport', {
        value: disabled,
        writable: false,
        configurable: false,
      });
      return;
    } catch (e) {
    }
    try {
      handlers.viewport = disabled;
      return;
    } catch (e) {
    }
    try {
      handlers.viewport.postMessage = disabled.postMessage;
    } catch (e) {
    }
  } catch (e) {
    /* The native handler, when it is still there, is additionally dropped by
     * each backend's origin policy. Nothing here may throw into the page. */
  }
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quote_cannot_end_the_literal_early() {
        // The message is interpolated into a script, so this is the difference
        // between a delivered message and a syntax error.
        assert_eq!(string_literal(r#"a"b"#), r#""a\"b""#);
        assert_eq!(string_literal(r"a\b"), r#""a\\b""#);
    }

    #[test]
    fn newlines_are_escaped() {
        assert_eq!(string_literal("a\nb"), r#""a\nb""#);
        assert_eq!(string_literal("a\r\nb"), r#""a\r\nb""#);
    }

    #[test]
    fn the_javascript_only_line_terminators_are_escaped() {
        // Legal inside a JSON string, but they end a line in JavaScript — so
        // interpolating one raw truncates the statement.
        assert_eq!(string_literal("a\u{2028}b"), "\"a\\u2028b\"");
        assert_eq!(string_literal("a\u{2029}b"), "\"a\\u2029b\"");
    }

    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(string_literal("a\u{1}b"), "\"a\\u0001b\"");
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        assert_eq!(
            string_literal(r#"{"type":"view.layout","id":1}"#),
            r#""{\"type\":\"view.layout\",\"id\":1}""#
        );
    }

    #[test]
    fn the_shim_defines_what_the_shell_reaches_for() {
        // data/shell/state.js:13 reads exactly this path.
        assert!(BRIDGE_SHIM.contains("window.webkit.messageHandlers.viewport"));
        assert!(BRIDGE_SHIM.contains("postMessage"));
    }

    #[test]
    fn the_shim_stringifies_objects_like_webkit_did() {
        assert!(BRIDGE_SHIM.contains("JSON.stringify(message)"));
    }

    #[test]
    fn the_shim_returns_before_installing_anything_in_a_subframe() {
        // The whole security property of the shim: an iframe gets no bridge.
        assert!(BRIDGE_SHIM.contains("window.top !== window"));
    }

    #[test]
    fn the_subframe_guard_replaces_the_native_handler() {
        assert!(SUBFRAME_GUARD.contains("window.top === window"));
        assert!(SUBFRAME_GUARD.contains("messageHandlers"));
        assert!(SUBFRAME_GUARD.contains("Object.defineProperty"));
    }

    #[test]
    fn file_and_loopback_origins_are_local() {
        for url in [
            "file:///usr/share/viewport/shell/index.html",
            "http://localhost:3000/",
            "https://localhost/",
            "http://127.0.0.1:8080/shell",
            "http://[::1]:3000",
            "https://[::1]",
        ] {
            assert!(is_local_shell_url(url), "{url}");
        }
    }

    #[test]
    fn remote_and_deceptive_origins_are_not_local() {
        for url in [
            "https://example.com/",
            "http://localhost.evil.example/",
            "http://evil.example/?localhost",
            "http://localhost@evil.example/",
            "http://127.0.0.1.evil.example/",
            "http://127.0.0.2/",
            "data:text/html,<script>",
            "/usr/share/viewport/shell/index.html",
            "",
        ] {
            assert!(!is_local_shell_url(url), "{url}");
        }
    }

    /// The shape both engines have to produce, spelled out.
    ///
    /// Written as a literal rather than built from the same helper the code
    /// uses, because a test that reuses the implementation cannot notice the
    /// implementation changing.
    #[test]
    fn the_dispatch_script_is_the_one_the_shell_listens_for() {
        assert_eq!(
            dispatch(r#"{"type":"view.added","id":1}"#),
            "window.dispatchEvent(new CustomEvent('viewport',\
             {detail:JSON.parse(\"{\\\"type\\\":\\\"view.added\\\",\\\"id\\\":1}\")}));"
        );
    }
}
