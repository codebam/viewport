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
