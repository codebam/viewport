// SPDX-License-Identifier: MIT
//
// The JSON protocol between the Viewport compositor and its web shell.
//
// This crate is deliberately free of any dependency on Smithay, Servo or a
// Wayland connection. The protocol is the fixed contract that `data/shell/*.js`
// was written against and that tests/shell.test.js exercises, so it needs to
// stay testable without a compositor to run it in.
//
// Ports src/ipc.c.

pub mod event;
pub mod geometry;
pub mod js;
pub mod request;

pub use event::{CastSource, Event};
pub use geometry::{Box, PartialBox, Transform};
pub use request::Request;

/// A message the compositor refused, in the shape the C build reports it.
///
/// These are not internal errors — each one is delivered back over IPC as an
/// [`Event::Error`], to the client that caused it where there is one and
/// broadcast otherwise.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The bytes were not JSON. `message` is the parser's own complaint.
    #[error("malformed IPC message: {message}")]
    Malformed { message: String },

    /// Valid JSON, but an array, a string, a number — anything but an object.
    #[error("JSON root is not an object")]
    NotAnObject,

    /// Absent, or present and not a string.
    ///
    /// The C build guards this explicitly because it once did not: `{"type":5}`,
    /// `{"type":null}`, `{"type":{}}` and `{"type":true}` all satisfied a
    /// `json_object_has_member()` gate and then reached a `strcmp()` on the NULL
    /// the getter handed back — a page one `postMessage()` away from killing
    /// the compositor and every window it held (`src/ipc.c:1476`).
    #[error("missing or non-string 'type'")]
    MissingType,

    /// A well-formed message naming a type with no handler.
    #[error("unknown IPC message type '{name}'")]
    UnknownType { name: String },

    /// The type is known but the body did not fit it.
    #[error("{name}: {message}")]
    BadBody { name: String, message: String },
}

impl ParseError {
    /// The `context` member of the error event this becomes.
    ///
    /// Everything that fails before dispatch is reported against `"ipc"`;
    /// once a type name is known, the error is reported against that name
    /// (`src/ipc.c:1502`).
    pub fn context(&self) -> &str {
        match self {
            Self::Malformed { .. } | Self::NotAnObject | Self::MissingType => "ipc",
            Self::UnknownType { name } | Self::BadBody { name, .. } => name,
        }
    }

    /// The error event to send back.
    pub fn to_event(&self) -> Event {
        Event::Error {
            context: self.context().to_owned(),
            message: self.to_string(),
        }
    }
}

/// Parse one message from the shell.
///
/// The staged checks exist to reproduce the C build's error messages exactly,
/// which is what makes the existing shell tests a usable oracle. A plain
/// `serde_json::from_str::<Request>` would collapse all four failures into one
/// unrecognisable serde string.
pub fn parse(bytes: &[u8]) -> Result<Request, ParseError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| ParseError::Malformed {
            message: e.to_string(),
        })?;

    let object = value.as_object().ok_or(ParseError::NotAnObject)?;

    let name = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or(ParseError::MissingType)?
        .to_owned();

    serde_json::from_value(value.clone()).map_err(|e| {
        // serde cannot tell "no such variant" from "that variant, malformed",
        // so the dispatch-table membership check has to be its own step.
        if is_known_type(&name) {
            ParseError::BadBody {
                name,
                message: e.to_string(),
            }
        } else {
            ParseError::UnknownType { name }
        }
    })
}

/// Every type this build dispatches: the C table (`src/ipc.c:1415`) and the
/// requests added since.
///
/// The later ones were missing here, and the only thing that reads this list is
/// the error message — so `{"type":"shell.command","args":[2]}` was reported as
/// an unknown message type rather than as the bad body it is, and the sender
/// went looking for a verb the compositor had never heard of.
fn is_known_type(name: &str) -> bool {
    matches!(
        name,
        "view.layout"
            | "view.visible"
            | "view.fullscreen"
            | "view.focus"
            | "view.close"
            | "view.opacity"
            | "view.query"
            | "shell.focus"
            | "shell.overview"
            | "session.save"
            | "session.query"
            | "notification.action"
            | "notification.dismiss"
            | "notification.expire"
            | "output.configure"
            | "output.hdr"
            | "output.confirm"
            | "output.active"
            | "output.query"
            | "output.test_add"
            | "output.test_remove"
            | "bind.add"
            | "quit"
            | "background.focus"
            | "screencast.rect"
            | "shell.overlay"
            | "workspace.list"
            | "shell.command"
            | "input.pointer"
            | "input.button"
            | "input.key"
    )
}

/// Serialise one message to the shell.
pub fn to_string(event: &Event) -> Result<String, serde_json::Error> {
    serde_json::to_string(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_json_is_reported_against_ipc() {
        let error = parse(b"{ nope").unwrap_err();
        assert_eq!(error.context(), "ipc");
        assert!(matches!(error, ParseError::Malformed { .. }));
    }

    #[test]
    fn a_non_object_root_is_rejected() {
        for raw in [b"[]".as_slice(), b"\"hi\"", b"5", b"null", b"true"] {
            assert_eq!(parse(raw).unwrap_err(), ParseError::NotAnObject);
        }
    }

    #[test]
    fn non_string_type_does_not_reach_dispatch() {
        // The exact shapes that used to crash the compositor.
        for raw in [
            r#"{"type":5}"#,
            r#"{"type":null}"#,
            r#"{"type":{}}"#,
            r#"{"type":true}"#,
            r#"{}"#,
        ] {
            assert_eq!(
                parse(raw.as_bytes()).unwrap_err(),
                ParseError::MissingType,
                "{raw}"
            );
        }
    }

    #[test]
    fn unknown_type_is_reported_against_itself() {
        let error = parse(br#"{"type":"view.teleport"}"#).unwrap_err();
        assert_eq!(error.context(), "view.teleport");
        assert_eq!(
            error.to_string(),
            "unknown IPC message type 'view.teleport'"
        );
    }

    #[test]
    fn a_known_type_with_a_bad_body_is_not_an_unknown_type() {
        // `id` is required and this one is a string.
        let error = parse(br#"{"type":"view.focus","id":"seven"}"#).unwrap_err();
        assert_eq!(error.context(), "view.focus");
        assert!(matches!(error, ParseError::BadBody { .. }), "{error:?}");
    }

    #[test]
    fn a_request_added_after_the_c_build_is_still_a_known_type() {
        // `args` is a list of strings, so this is a bad body — and it used to
        // come back as "unknown IPC message type 'shell.command'", which sent
        // whoever wrote it looking for a message that does exist.
        let error =
            parse(br#"{"type":"shell.command","command":"view.move","args":[2]}"#).unwrap_err();
        assert!(matches!(error, ParseError::BadBody { .. }), "{error:?}");
        assert_eq!(error.context(), "shell.command");
    }

    #[test]
    fn errors_become_error_events() {
        let event = ParseError::NotAnObject.to_event();
        assert_eq!(
            event,
            Event::Error {
                context: "ipc".into(),
                message: "JSON root is not an object".into(),
            }
        );
    }

    #[test]
    fn a_good_message_parses() {
        assert_eq!(
            parse(br#"{"type":"view.close","id":12}"#).unwrap(),
            Request::ViewClose { id: 12 }
        );
    }
}
