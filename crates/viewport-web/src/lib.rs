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
use std::path::Path;

#[cfg(feature = "wpe")]
pub mod webkit;
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

/// The script injected before any of the shell's own scripts, for an engine
/// with no `window.webkit.messageHandlers` of its own.
///
/// Moved to `viewport_ipc::js` when a second out-of-process shell needed it and
/// could not take this crate's dependency on GBM and EGL to get it. Re-exported
/// because it is part of this crate's published surface.
pub use viewport_ipc::js::BRIDGE_SHIM;

/// Characters that must be percent-encoded when a filesystem path is embedded
/// in a `file://` URL.
///
/// WebKit parses the URI before loading it, so URL syntax characters — the
/// fragment `#`, the query `?`, and `%` itself — would be read as structure
/// rather than path, and the characters RFC 3986 calls unsafe in a path
/// component would not survive the trip either. Control characters and every
/// non-ASCII byte are encoded on top of this set.
const FILE_PATH: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'%')
    .add(b'{')
    .add(b'}')
    .add(b'|')
    .add(b'\\')
    .add(b'^')
    .add(b'`');

/// Build a `file://` URL for a filesystem path, percent-encoding it.
///
/// A path is arbitrary bytes as far as the shell is concerned, but a URI is
/// not: a space or a `#` left raw would truncate the path WebKit sees, and a
/// shell installed under such a path would come up blank.
pub fn file_url(path: &Path) -> String {
    format!(
        "file://{}",
        percent_encoding::utf8_percent_encode(&path.to_string_lossy(), FILE_PATH)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_path_is_left_alone() {
        assert_eq!(
            file_url(Path::new("/usr/share/viewport/index.html")),
            "file:///usr/share/viewport/index.html"
        );
    }

    #[test]
    fn url_syntax_characters_are_escaped() {
        assert_eq!(
            file_url(Path::new("/opt/my shell/v1?page=2#top")),
            "file:///opt/my%20shell/v1%3Fpage=2%23top"
        );
    }

    #[test]
    fn a_percent_sign_is_escaped_itself() {
        assert_eq!(file_url(Path::new("/tmp/100%/x")), "file:///tmp/100%25/x");
    }

    #[test]
    fn non_ascii_is_utf8_percent_encoded() {
        assert_eq!(file_url(Path::new("/tmp/é")), "file:///tmp/%C3%A9");
    }
}
