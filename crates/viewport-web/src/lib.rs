// SPDX-License-Identifier: MIT
//
// The web engine behind the shell.
//
// Ports src/web.c, src/wpe_display.c, src/wpe_view.c and src/web_buffer.c —
// except that the engine underneath is Servo rather than WPE WebKit.
//
// Everything here is behind a trait on purpose. Swapping WebKit for Servo is
// the single largest bet in this rewrite, and the compositor should not be able
// to tell the difference: it hands over a size, gets back a DMA-BUF and a
// fence, and exchanges JSON. If Servo does not work out, only this crate
// changes.

use std::os::unix::io::OwnedFd;

pub mod dmabuf;

#[cfg(feature = "wpe")]
pub mod wpe;

use viewport_ipc::{Event, Request};

/// A frame the engine has finished painting.
///
/// The compositor never touches these pixels. The buffer is imported straight
/// into the scene and the fence is imported into a `drm_syncobj` timeline, so
/// the render waits on the GPU rather than the compositor blocking on the CPU.
#[derive(Debug)]
pub struct Frame {
    /// The exported DMA-BUF planes.
    pub planes: Vec<Plane>,
    /// `DRM_FORMAT_*`.
    pub format: u32,
    /// The DRM format modifier the buffer was allocated with.
    pub modifier: u64,
    pub width: u32,
    pub height: u32,

    /// The engine's rendering fence, signalled when the paint completes.
    ///
    /// `None` means the engine could not produce one and the frame is already
    /// complete; the compositor must not treat that as an error, only as a
    /// missed optimisation.
    pub fence: Option<OwnedFd>,
}

#[derive(Debug)]
pub struct Plane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// What the compositor wants to be told about.
///
/// The engine calls these; it does not own the compositor state, so everything
/// it can say is either "here is a frame" or "the page said something".
pub trait WebHandler {
    /// A new frame is ready.
    ///
    /// The engine will not paint frame N+1 until [`WebEngine::frame_done`] is
    /// called for frame N, which is what pins the shell's paint rate to real
    /// vblank instead of letting it free-run.
    fn frame(&mut self, frame: Frame);

    /// The page sent a message. Already parsed, because a malformed one is an
    /// error the shell needs told about rather than something to hand on.
    fn request(&mut self, request: Request);

    /// The page sent something that would not parse.
    fn malformed(&mut self, error: viewport_ipc::ParseError);
}

/// A web engine rendering the shell.
pub trait WebEngine {
    /// Load a URL, replacing whatever is showing.
    fn load(&mut self, url: &str) -> anyhow::Result<()>;

    /// Resize the page. Called when the output layout changes, since the shell
    /// spans the whole layout rather than one output.
    fn resize(&mut self, width: u32, height: u32) -> anyhow::Result<()>;

    /// Send a message to the page.
    ///
    /// This becomes `window.dispatchEvent(new CustomEvent('viewport', {detail:
    /// ...}))` in the page, exactly as `src/web.c:48` does it, so the shell's
    /// existing listener needs no change.
    fn post(&mut self, event: &Event) -> anyhow::Result<()>;

    /// Acknowledge the frame most recently handed to [`WebHandler::frame`],
    /// releasing the engine to paint the next one. Called from the output's
    /// frame handler.
    fn frame_done(&mut self);

    /// Drop caches and reload, ignoring the HTTP cache. The escape hatch for a
    /// shell being edited live.
    fn reload(&mut self) -> anyhow::Result<()>;
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
    fn the_shim_defines_what_the_shell_reaches_for() {
        // data/shell/state.js:13 reads exactly this path.
        assert!(BRIDGE_SHIM.contains("window.webkit.messageHandlers.viewport"));
        assert!(BRIDGE_SHIM.contains("postMessage"));
    }

    #[test]
    fn the_shim_stringifies_objects_like_webkit_did() {
        assert!(BRIDGE_SHIM.contains("JSON.stringify(message)"));
    }
}
