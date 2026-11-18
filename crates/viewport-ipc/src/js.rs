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

/// The name the page calls to reach the compositor.
///
/// [`BRIDGE_SHIM`] wraps it in `window.webkit.messageHandlers.viewport`, which
/// is the name the shell was written against. The DevTools backends register
/// it with `Runtime.addBinding`; the Servo backends' bridge script defines it
/// as the sender.
pub const BINDING: &str = "__viewport_send";

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

/// The one outbound bridge script, shared by every backend that has to build
/// it from the web platform.
///
/// Both Servo backends inject this before the page's own scripts. The sender
/// is a `fetch` the backend supplies as `post`; [`BRIDGE_SHIM`] wraps it in the
/// `window.webkit.messageHandlers.viewport` name `data/shell/state.js` calls;
/// `tail` is whatever the backend needs after the shim (servoshell's `/events`
/// long poll, nothing for the in-process engine). `vars` carries any extra
/// declarations the transport needs, such as servoshell's `ORIGIN`.
///
/// The pieces are spliced in a fixed order because the order is part of the
/// contract: the subframe guard and the origin gate run before anything is
/// installed, `vars` and the token come next, the batching machine defines
/// `window.__viewport_send`, and only then is the shim interpolated — the shim
/// reads the sender at install time and gives up loudly if it is not there.
///
/// `token` is quoted with [`string_literal`] rather than interpolated raw, so
/// a token that could end a literal early cannot. `vars`, `post` and `tail`
/// may each be empty.
pub fn bridge_script(token: &str, vars: &str, post: &str, tail: &str) -> String {
    format!(
        r#"(function () {{
  'use strict';

  /* A frame inside the shell is not the shell. The script is injected into
     every document, including one the shell navigates or reloads to, so this
     is also what keeps a remote document from reaching the transport below
     after a navigation to an origin the policy does not allow. */
  if (window.top !== window) {{
    return;
  }}

{vars}  var TOKEN = {token};
  var ALLOW_REMOTE = {allow_remote};

  function bridgeOriginAllowed() {{
    try {{
      var location = window.location;
      var protocol = (location.protocol || '').toLowerCase();
      var host = (location.hostname || '').toLowerCase();
      if (protocol === 'file:') {{
        /* `file://host/path` with a real host is a UNC-style remote path on
           some platforms, so only an empty or loopback authority is allowed —
           the same rule `is_local_shell_url` applies on the Rust side. */
        return host === '' || host === 'localhost' || host === '127.0.0.1' ||
               host === '::1' || host === '[::1]';
      }}
      if (protocol !== 'http:' && protocol !== 'https:') {{
        return false;
      }}
      return host === 'localhost' || host === '127.0.0.1' ||
             host === '::1' || host === '[::1]';
    }} catch (e) {{
      return false;
    }}
  }}

  if (!ALLOW_REMOTE && !bridgeOriginAllowed()) {{
    console.error('viewport: the bridge is disabled on this origin; ' +
                  'set VIEWPORT_ALLOW_REMOTE_SHELL=1 to allow a remote shell');
    return;
  }}

  /* Outbound messages are batched to one request per task rather than one
     request each, and the reason is a measurement rather than tidiness: the
     shell sends a message per window per frame while anything is animating,
     and Servo's script thread was spending its frame on ~190 fetches a second —
     26 frames a second painted against the 122 the engine reaches on a page
     that does not talk to a compositor. A microtask flush adds no latency of
     its own: every message produced by one frame's handlers is still delivered
     before the frame ends, as one request with a line per message. */
  var pending = [];
  var scheduled = false;

  /* `batch=1` is what says the payload is a line per message. Without it the
     payload is one message whatever is in it, which is what a message carrying
     a newline is sent as: the separator cannot corrupt a body nobody splits. */
{post}
  function flush() {{
    scheduled = false;
    if (pending.length === 0) {{
      return;
    }}
    var batch = pending;
    pending = [];
    if (batch.length === 1) {{
      post(batch[0], false);
      return;
    }}
    post(batch.join('\n'), true);
  }}

  window.__viewport_send = function (message) {{
    /* Nothing the shell sends carries a newline — every message is
       `JSON.stringify` output, which escapes them — but one that did would
       corrupt the message after it rather than itself, which is the hardest
       kind of bug to find. It goes on its own, unbatched and unsplit. */
    if (message.indexOf('\n') !== -1) {{
      flush();
      post(message, false);
      return;
    }}
    pending.push(message);
    if (!scheduled) {{
      scheduled = true;
      Promise.resolve().then(flush);
    }}
  }};

  /* The page going away does not run a microtask that was only scheduled, so
     whatever is still queued is sent while there is still a page to send it. */
  window.addEventListener('pagehide', flush);
  window.addEventListener('beforeunload', flush);

{shim}
{tail}}})();
"#,
        token = string_literal(token),
        allow_remote = remote_shell_allowed(),
        vars = vars,
        post = post,
        shim = BRIDGE_SHIM,
        tail = tail,
    )
}

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
    fn the_shim_wraps_the_binding_the_backends_register() {
        // The name the DevTools backends register and the Servo backends
        // define is the one the shim reads; a drift between them is a shell
        // with no outbound bridge.
        assert!(BRIDGE_SHIM.contains(BINDING));
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

    /// The shared bridge is what both Servo backends inject, so its gates are
    /// pinned here rather than in either backend alone.
    #[test]
    fn the_bridge_script_refuses_subframes() {
        let script = bridge_script("t", "", "", "");
        assert!(
            script.contains("if (window.top !== window) {\n    return;\n  }"),
            "{script}"
        );
    }

    #[test]
    fn the_bridge_script_file_branch_allows_only_empty_or_loopback_hosts() {
        let script = bridge_script("t", "", "", "");
        // The UNC rule is named and pointed at the classifier it mirrors.
        assert!(script.contains("UNC"), "{script}");
        assert!(script.contains("is_local_shell_url"), "{script}");
        let file_branch = script
            .split("if (protocol === 'file:')")
            .nth(1)
            .expect("the file branch");
        let file_branch = file_branch.split("if (protocol !==").next().unwrap();
        assert!(file_branch.contains("host === ''"), "{script}");
        assert!(file_branch.contains("host === 'localhost'"), "{script}");
        assert!(file_branch.contains("host === '127.0.0.1'"), "{script}");
        assert!(file_branch.contains("host === '::1'"), "{script}");
        assert!(file_branch.contains("host === '[::1]'"), "{script}");
        // The drift was `return true;` for any file: URL; it must be gone.
        assert!(!file_branch.contains("return true;"), "{script}");
    }

    #[test]
    fn the_bridge_script_defines_the_sender_before_the_shim() {
        let script = bridge_script("t", "", "", "");
        let sender = script.find("window.__viewport_send").expect("the sender");
        let shim = script
            .find("window.webkit.messageHandlers.viewport = handler")
            .expect("the shim installs the name");
        assert!(sender < shim, "{script}");
    }

    #[test]
    fn the_bridge_script_quotes_the_token_as_a_literal() {
        // A quote in the token would end a raw interpolation early; the
        // literal escapes it. Spelled out, not rebuilt from string_literal.
        let script = bridge_script("a\"b", "", "", "");
        assert!(script.contains(r#"var TOKEN = "a\"b""#), "{script}");
    }

    #[test]
    fn the_bridge_script_gates_on_allow_remote() {
        let script = bridge_script("t", "", "", "");
        assert!(
            script.contains("if (!ALLOW_REMOTE && !bridgeOriginAllowed())"),
            "{script}"
        );
        assert!(script.contains("VIEWPORT_ALLOW_REMOTE_SHELL=1"), "{script}");
    }

    #[test]
    fn the_bridge_script_keeps_the_batching_and_the_newline_escape_hatch() {
        let script = bridge_script("t", "", "", "");
        assert!(script.contains("Promise.resolve().then(flush)"), "{script}");
        assert!(script.contains(r"post(batch.join('\n'), true)"), "{script}");
        assert!(
            script.contains(r"if (message.indexOf('\n') !== -1)"),
            "{script}"
        );
        assert!(
            script.contains("window.addEventListener('pagehide', flush)"),
            "{script}"
        );
        assert!(
            script.contains("window.addEventListener('beforeunload', flush)"),
            "{script}"
        );
    }

    #[test]
    fn the_bridge_script_splices_the_backends_pieces() {
        let script = bridge_script(
            "t",
            "  var ORIGIN = 'http://127.0.0.1:1';\n",
            "  function post(m, b) {}\n",
            "  pump();\n",
        );
        assert!(
            script.contains("var ORIGIN = 'http://127.0.0.1:1';"),
            "{script}"
        );
        assert!(script.contains("function post(m, b) {}"), "{script}");
        assert!(script.contains("pump();"), "{script}");
    }
}
