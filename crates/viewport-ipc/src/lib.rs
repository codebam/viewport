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

pub use event::{CastSource, Event, GestureKind, ShortcutRow};
pub use geometry::{Box, PartialBox, Transform};
pub use request::{GestureCaptureSpec, Request};

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
/// A valid message is parsed once, straight into [`Request`]; only when that
/// fails does the staged path below run. That path exists to reproduce the C
/// build's error messages exactly, which is what makes the existing shell
/// tests a usable oracle. A plain `serde_json::from_str::<Request>` error would
/// collapse all four failures into one unrecognisable serde string.
pub fn parse(bytes: &[u8]) -> Result<Request, ParseError> {
    // Fast path: the common case is a valid request, and it can be decoded
    // straight into what dispatch needs. The staged checks below are the
    // error-message oracle, so they can wait for a failure — which is what
    // keeps a successful request from being decoded twice.
    if let Ok(request) = serde_json::from_slice::<Request>(bytes) {
        return Ok(request);
    }

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

    // The staged path is the oracle for *why* a message was rejected, so it
    // runs only after the fast path above has already rejected the bytes. From
    // the bytes again rather than from the parsed value. `from_value` would
    // deep-clone the whole document per rejected message — up to the framing
    // cap — purely so the staged checks above could keep it alive long enough
    // to name the type in an error. They only need the name, and that is
    // already owned. Nothing is lost by reading the bytes a second time:
    // `from_slice` reports the line and column of a bad body where
    // `from_value`, which has no positions to point at, reports neither.
    serde_json::from_slice(bytes).map_err(|e| {
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
            | "view.maximized"
            | "view.minimized"
            | "view.focus"
            | "view.close"
            | "view.opacity"
            | "view.opacity_rule"
            | "view.capture"
            | "view.tearing"
            | "view.blur"
            | "view.idle_inhibit"
            | "view.confine_pointer"
            | "view.anr_resolve"
            | "view.decorations"
            | "view.query"
            | "shell.focus"
            | "shell.overview"
            | "session.save"
            | "session.query"
            | "session.lock"
            | "session.lock.drawn"
            | "session.unlock"
            | "notification.action"
            | "notification.dismiss"
            | "notification.expire"
            | "notification.list"
            | "notification.forget"
            | "tray.activate"
            | "tray.scroll"
            | "clipboard.query"
            | "clipboard.paste"
            | "clipboard.forget"
            | "launcher.query"
            | "launcher.launch"
            | "mpris.control"
            | "ai.login"
            | "power.profile"
            | "power.action"
            | "osk.key"
            | "network.scan"
            | "network.connect"
            | "network.disconnect"
            | "network.radio"
            | "bluetooth.power"
            | "bluetooth.scan"
            | "bluetooth.device"
            | "tray.menu.click"
            | "tray.menu.closed"
            | "output.configure"
            | "output.hdr"
            | "output.confirm"
            | "output.active"
            | "gesture.capture"
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
            | "shell.exec"
            | "status.refresh"
            | "status.volume"
            | "input.pointer"
            | "input.button"
            | "input.key"
            | "input.scroll"
            | "config.gaps"
            | "config.border"
            | "config.wallpaper"
            | "config.wallpaper_options"
            | "config.dark_mode"
            | "config.game_mode"
            | "config.cursor"
            | "config.motion"
            | "config.group"
            | "config.clock"
            | "config.bar"
            | "config.bar_blur"
            | "config.bar_widgets"
            | "config.bar_items"
            | "config.logo"
            | "config.tutorial"
            | "config.tiling_mode"
            | "config.layout"
            | "config.layout_extensions"
            | "config.focus_crosses_outputs"
            | "config.osk"
            | "config.tray"
            | "config.clipboard_history"
            | "config.icon_theme"
            | "config.theme"
            | "config.rules"
            | "config.layer_rules"
            | "config.decorations"
            | "config.workspaces"
            | "config.opacity"
            | "config.idle"
            | "config.lid"
            | "config.keyboard"
            | "config.input"
            | "config.notifications"
            | "config.magnify"
            | "config.save"
            | "output.revert"
    )
}

/// Every type `Request` accepts, read off the enum itself.
///
/// Only the tests use it: `is_known_type` is a `matches!` because it is on the
/// path every message takes, and a list written twice is a list that drifts —
/// `config.border` and `config.wallpaper` were both missing from it, which
/// costs nothing until a malformed one of those arrives and is reported as a
/// type nobody has heard of rather than as the bad body it is.
#[cfg(test)]
fn declared_types() -> Vec<String> {
    let source = include_str!("request.rs");
    let mut names = Vec::new();
    let mut lines = source.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.trim().strip_prefix("#[serde(rename = \"") else {
            continue;
        };
        let Some(name) = rest.split('"').next() else {
            continue;
        };
        // A variant follows its rename, and a variant is capitalised where a
        // field is not — which is the whole difference between a message type
        // and a renamed field inside one.
        let follows = lines.peek().map(|l| l.trim()).unwrap_or_default();
        let is_variant = follows.starts_with(|c: char| c.is_ascii_uppercase());
        if is_variant {
            names.push(name.to_owned());
        }
    }
    names
}

/// The serde fields of one struct, read off the source.
///
/// `#[serde(flatten)]` splices the flattened struct's fields in: its own name
/// never reaches the wire, `ViewLayout`'s `box_` is the one here.
#[cfg(test)]
fn struct_field_names(sources: &[&str], name: &str) -> Vec<String> {
    let header = format!("pub struct {name} {{");
    for source in sources {
        let mut lines = source.lines();
        while let Some(line) = lines.next() {
            if line.trim() != header {
                continue;
            }
            let mut fields = Vec::new();
            let mut flatten = false;
            for line in lines.by_ref() {
                let line = line.trim();
                if line == "}" {
                    break;
                }
                if line == "#[serde(flatten)]" {
                    flatten = true;
                    continue;
                }
                if line.is_empty() || line.starts_with("///") || line.starts_with("#[") {
                    continue;
                }
                let Some(rest) = line.strip_prefix("pub ") else {
                    continue;
                };
                let Some((field, ty)) = rest.split_once(':') else {
                    continue;
                };
                let ty = ty.trim().trim_end_matches(',');
                if flatten {
                    fields.extend(struct_field_names(sources, ty));
                    flatten = false;
                } else {
                    fields.push(field.trim().to_owned());
                }
            }
            return fields;
        }
    }
    Vec::new()
}

/// Every variant of one internally-tagged enum, with the serde fields it
/// deserialises.
///
/// `struct_sources` are searched for the struct behind a newtype variant and
/// for a flattened field; `event.rs` keeps its payloads beside the enum,
/// `request.rs` needs `geometry.rs` for `PartialBox`.
#[cfg(test)]
fn enum_variant_fields(
    source: &str,
    enum_name: &str,
    struct_sources: &[&str],
) -> Vec<(String, Vec<String>)> {
    let header = format!("pub enum {enum_name} {{");
    let mut variants = Vec::new();
    let mut lines = source.lines().peekable();
    let mut inside = false;
    while let Some(line) = lines.next() {
        let line = line.trim();
        if !inside {
            inside = line == header;
            continue;
        }
        if line == "}" {
            break;
        }
        let Some(rest) = line.strip_prefix("#[serde(rename = \"") else {
            continue;
        };
        let Some(name) = rest.split('"').next() else {
            continue;
        };
        // The declaration follows the doc comment and attributes.
        let mut decl = String::new();
        for next in lines.by_ref() {
            let next = next.trim();
            if next.is_empty()
                || next.starts_with("///")
                || next.starts_with("//")
                || next.starts_with("#[")
            {
                continue;
            }
            decl = next.to_owned();
            break;
        }
        let fields = declaration_field_names(&decl, struct_sources, &mut lines);
        variants.push((name.to_owned(), fields));
    }
    variants
}

/// The fields of one variant declaration, consuming its body when it is
/// written across lines.
#[cfg(test)]
fn declaration_field_names<'a>(
    decl: &str,
    struct_sources: &[&str],
    lines: &mut impl Iterator<Item = &'a str>,
) -> Vec<String> {
    let Some(open) = decl.find(['{', '(', ',']) else {
        return Vec::new();
    };
    let rest = &decl[open..];
    if let Some(inner) = rest.strip_prefix('(') {
        let name = inner.split(')').next().unwrap_or_default().trim();
        let name = name
            .strip_prefix("Box<")
            .and_then(|name| name.strip_suffix('>'))
            .unwrap_or(name);
        return struct_field_names(struct_sources, name);
    }
    if rest.starts_with(',') {
        return Vec::new();
    }
    let mut text = rest.trim_start_matches('{').to_owned();
    let mut done = false;
    if let Some(end) = text.find('}') {
        text.truncate(end);
        done = true;
    }
    let mut fields = Vec::new();
    let mut flatten = false;
    loop {
        // An attribute is a whole line: splitting one on its own commas would
        // read the `::` in `Option::is_none` as a field boundary.
        let whole = text.trim();
        if whole == "#[serde(flatten)]" {
            flatten = true;
        } else if !(whole.starts_with("///") || whole.starts_with("//") || whole.starts_with("#["))
        {
            for part in text.split(',') {
                let part = part.trim();
                if part.is_empty() || part.starts_with("///") || part.starts_with("//") {
                    continue;
                }
                if part == "#[serde(flatten)]" {
                    flatten = true;
                    continue;
                }
                if part.starts_with("#[") {
                    continue;
                }
                let Some((field, ty)) = part.split_once(':') else {
                    continue;
                };
                let ty = ty.trim();
                if flatten {
                    fields.extend(struct_field_names(struct_sources, ty));
                    flatten = false;
                } else {
                    fields.push(field.trim().to_owned());
                }
            }
        }
        if done {
            break;
        }
        let Some(next) = lines.next() else {
            break;
        };
        let next = next.trim();
        if next == "}" || next == "}," {
            break;
        }
        text = next.to_owned();
    }
    fields
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

    /// The two lists say the same thing, or a malformed message is reported as
    /// an unknown one.
    ///
    /// `is_known_type` exists to tell "no such variant" from "that variant,
    /// malformed", which serde cannot — so a type missing from it turns a
    /// useful error into a wrong one. Two were missing when this was written.
    #[test]
    fn every_request_type_is_known() {
        let declared = declared_types();
        assert!(
            declared.len() >= 35,
            "the enum was not read properly: {declared:?}"
        );
        for name in declared {
            assert!(is_known_type(&name), "{name} is not in is_known_type");
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
    fn malformed_view_capture_is_a_bad_body_of_a_known_type() {
        for raw in [
            br#"{"type":"view.capture","id":7}"#.as_slice(),
            br#"{"type":"view.capture","id":7,"capture":1}"#,
        ] {
            let error = parse(raw).unwrap_err();
            assert_eq!(error.context(), "view.capture");
            assert!(matches!(error, ParseError::BadBody { .. }), "{error:?}");
        }
    }

    #[test]
    fn a_bad_body_still_says_what_was_wrong_with_it() {
        // Parsed from the original bytes rather than a cloned value, and the
        // complaint keeps the field, the type and the position either way —
        // which is what makes it usable at a prompt.
        let error = parse(br#"{"type":"view.focus","id":"seven"}"#).unwrap_err();
        let ParseError::BadBody { name, message } = &error else {
            panic!("should be a bad body, not {error:?}");
        };
        assert_eq!(name, "view.focus");
        assert!(message.contains("invalid type"), "{message}");
        assert!(message.contains("seven"), "{message}");
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

    /// The fast path's main customer is `view.layout`, the message the shell
    /// sends for every window on every frame. A full body must still land on
    /// exactly the value the staged path used to produce.
    #[test]
    fn a_valid_full_request_parses_in_one_pass() {
        let raw = br#"{"type":"view.layout","id":3,"x":10,"y":20,"width":800,"height":600,"scale":0.5,"clip":{"height":40},"floating":true}"#;
        assert_eq!(
            parse(raw).unwrap(),
            Request::ViewLayout(request::ViewLayout {
                id: 3,
                box_: geometry::PartialBox {
                    x: Some(10),
                    y: Some(20),
                    width: Some(800),
                    height: Some(600),
                },
                scale: Some(0.5),
                clip: Some(geometry::PartialBox {
                    x: None,
                    y: None,
                    width: None,
                    height: Some(40),
                }),
                frame: None,
                floating: true,
                square: false,
            })
        );
    }

    /// The `| message | payload |` rows of `docs/ipc.md`, keyed by the message
    /// name in the first cell. Prose and the small key tables inside a section
    /// come out with whatever their first cell says, which no message is
    /// named, so they are never looked up.
    ///
    /// A name can have a row in each direction — `session.lock` is one — so
    /// each key keeps its rows and a variant is only checked against them.
    fn documented_rows(doc: &str) -> std::collections::BTreeMap<String, Vec<String>> {
        let mut rows: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for line in doc.lines() {
            let Some(rest) = line.trim().strip_prefix("| `") else {
                continue;
            };
            let Some((name, rest)) = rest.split_once('`') else {
                continue;
            };
            let Some(payload) = rest.trim_start().strip_prefix('|') else {
                continue;
            };
            let payload = payload.strip_suffix('|').unwrap_or(payload);
            rows.entry(name.to_owned())
                .or_default()
                .push(payload.trim().to_owned());
        }
        rows
    }

    /// Whether `row` names `field` as a word, rather than as part of a longer
    /// one. The table writes every name in backticks, so the neighbours are
    /// punctuation, but this keeps `id` from matching the `id` inside `app_id`.
    fn mentions(row: &str, field: &str) -> bool {
        row.match_indices(field).any(|(at, _)| {
            let before = row[..at].chars().next_back();
            let after = row[at + field.len()..].chars().next();
            let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '_');
            boundary(before) && boundary(after)
        })
    }

    /// Rows that describe a message in prose rather than listing its members.
    ///
    /// `config` names the members the shell draws and leaves the rest of the
    /// settings to `docs/configuration.md`; every other row is a field list.
    /// Without this the test would demand a second copy of the config
    /// reference inside the message table.
    const SUMMARY_ROWS: &[&str] = &["config"];

    /// The one place an alternative shell can learn what a message carries is
    /// `docs/ipc.md`, so a member added to `Event` or `Request` and not named
    /// there is a member nobody outside this repository can use. This reads
    /// both enums' serde fields and both tables, and treats a row that does not
    /// exist yet as unwritten rather than wrong.
    #[test]
    fn every_documented_message_names_the_fields_it_sends() {
        let doc = include_str!("../../../docs/ipc.md");
        let rows = documented_rows(doc);
        let mut checked = 0;
        let mut missing = Vec::new();
        for (source, enum_name, structs) in [
            (
                include_str!("event.rs"),
                "Event",
                vec![include_str!("event.rs")],
            ),
            (
                include_str!("request.rs"),
                "Request",
                vec![include_str!("request.rs"), include_str!("geometry.rs")],
            ),
        ] {
            for (message, fields) in enum_variant_fields(source, enum_name, &structs) {
                if SUMMARY_ROWS.contains(&message.as_str()) {
                    continue;
                }
                let Some(rows) = rows.get(&message) else {
                    continue;
                };
                checked += 1;
                if rows
                    .iter()
                    .any(|row| fields.iter().all(|field| mentions(row, field)))
                {
                    continue;
                }
                let before = missing.len();
                for field in fields {
                    if !rows.iter().any(|row| mentions(row, &field)) {
                        missing.push(format!("{message}: {field}"));
                    }
                }
                if missing.len() == before {
                    missing.push(format!("{message}: no single row names them all"));
                }
            }
        }
        assert!(
            checked >= 100,
            "the tables were not read properly: {checked} rows checked"
        );
        assert!(missing.is_empty(), "rows do not name: {missing:?}");
    }
}
