// SPDX-License-Identifier: GPL-3.0-or-later
//
// org.freedesktop.impl.portal.Screenshot.
//
// Answering screenshot requests from desktop portals and applications.
// Whole outputs are served today; a request that asks the user to choose what
// is captured is refused rather than quietly downgraded to an output, because
// the picker lives in the compositor loop and not behind this interface.

use std::collections::HashMap;

use smithay::output::Output;
use zvariant::{ObjectPath, OwnedValue};

const RESPONSE_SUCCESS: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_FAILED: u32 = 2;

/// A screenshot request queued for the renderer.
#[derive(Debug)]
pub struct PendingScreenshot {
    pub output: Option<Output>,
    pub window_id: Option<u32>,
    pub reply: async_channel::Sender<Result<String, String>>,
}

/// Messages from the D-Bus portal interface to the compositor loop.
#[derive(Debug)]
pub enum Message {
    Capture {
        interactive: bool,
        modal: bool,
        reply: async_channel::Sender<Result<String, String>>,
    },
}

/// What a screenshot call's options ask this interface to do.
///
/// `interactive` asks for a picker, which is not behind this method: the
/// caller wanted to choose what is captured, and answering with the whole
/// output is not the same request. `None` refuses. `modal` only qualifies the
/// picker, so it is carried when the request is answerable and ignored
/// otherwise.
///
/// The options are advisory and the portal frontend is trusted for their
/// shape, so a wrong D-Bus type reads as an absent option rather than a
/// failure.
fn capture_options(options: &HashMap<String, OwnedValue>) -> Option<(bool, bool)> {
    let interactive = options
        .get("interactive")
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false);
    if interactive {
        return None;
    }
    let modal = options
        .get("modal")
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false);
    Some((false, modal))
}

#[derive(Clone)]
pub struct Screenshot {
    sender: smithay::reexports::calloop::channel::Sender<Message>,
    sessions: crate::screencast::portal::Sessions,
}

impl Screenshot {
    pub fn new(
        sender: smithay::reexports::calloop::channel::Sender<Message>,
        sessions: crate::screencast::portal::Sessions,
    ) -> Self {
        Self { sender, sessions }
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Screenshot")]
impl Screenshot {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }

    /// Capture a screenshot.
    async fn screenshot(
        &self,
        _handle: ObjectPath<'_>,
        _session_handle: ObjectPath<'_>,
        _app_id: &str,
        _parent_window: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        if !crate::screencast::portal::called_by_frontend(&self.sessions, "screenshot", &header) {
            return (RESPONSE_CANCELLED, HashMap::new());
        }
        let Some((interactive, modal)) = capture_options(&options) else {
            // No picker is behind this method yet. Refusing is the honest
            // answer; capturing the active output would hand the application
            // something it did not ask for.
            tracing::warn!("screenshot: refusing an interactive request with no picker to show");
            return (RESPONSE_FAILED, HashMap::new());
        };

        let (reply_tx, reply_rx) = async_channel::bounded(1);
        let msg = Message::Capture {
            interactive,
            modal,
            reply: reply_tx,
        };

        if self.sender.send(msg).is_err() {
            return (RESPONSE_FAILED, HashMap::new());
        }

        match reply_rx.recv().await {
            Ok(Ok(uri)) => {
                let mut results = HashMap::new();
                results.insert("uri".to_owned(), OwnedValue::from(zvariant::Str::from(uri)));
                (RESPONSE_SUCCESS, results)
            }
            Ok(Err(_)) => (RESPONSE_CANCELLED, HashMap::new()),
            Err(_) => (RESPONSE_FAILED, HashMap::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(entries: &[(&str, OwnedValue)]) -> HashMap<String, OwnedValue> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    #[test]
    fn an_interactive_request_is_refused_rather_than_downgraded() {
        // The caller asked to choose; the whole output is not that request.
        assert_eq!(
            capture_options(&options(&[("interactive", OwnedValue::from(true))])),
            None
        );
        assert_eq!(
            capture_options(&options(&[
                ("interactive", OwnedValue::from(true)),
                ("modal", OwnedValue::from(true)),
            ])),
            None
        );
    }

    #[test]
    fn a_plain_or_modal_request_is_an_output_capture() {
        assert_eq!(capture_options(&HashMap::new()), Some((false, false)));
        assert_eq!(
            capture_options(&options(&[("modal", OwnedValue::from(true))])),
            Some((false, true))
        );
    }

    #[test]
    fn a_wrongly_typed_option_is_ignored_like_a_missing_one() {
        // Advisory options; a non-bool must not read as "interactive".
        assert_eq!(
            capture_options(&options(&[("interactive", OwnedValue::from(1u32))])),
            Some((false, false))
        );
    }
}
