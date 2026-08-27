// SPDX-License-Identifier: GPL-3.0-or-later
//
// org.freedesktop.impl.portal.Screenshot.
//
// Answering screenshot requests from desktop portals and applications.
// A screenshot of an output, a window, or an interactive region.

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
        let interactive = options
            .get("interactive")
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false);
        let modal = options
            .get("modal")
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false);

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
