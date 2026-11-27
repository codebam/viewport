# `crates/viewport-ipc` — Wire Protocol & Page Bridge

- **License**: MIT (must stay free of Smithay/Wayland/GPL deps).
- **Role**: Shared JSON protocol between `viewport`, external `viewport msg` clients, and all 6 shell backends.

## Modules
- `lib.rs`: `parse(&[u8]) -> Result<Request, ParseError>`, `to_string(&Event)`, `is_known_type` (`matches!` table on fast-failure path), `ParseError` (`Malformed`, `NotAnObject`, `MissingType`, `UnknownType`, `BadBody`) matching C-era error context/messages.
  - Contains source-reading contract tests: `every_request_type_is_known` and `every_documented_message_names_the_fields_it_sends` (parses `docs/ipc.md` tables against `event.rs` and `request.rs`!).
- `request.rs`: `Request` enum (110 `#[serde(rename = "...")]` variants), `ViewLayout`, `OutputConfigure`, `ModeRequest`, `GestureCaptureSpec`, `Workspace`, `OverlayRect`, `redact_secrets` (redacts `SessionUnlock.password`, `NetworkConnect.passphrase` in `Debug`).
- `event.rs`: `Event` enum (43 `#[serde(rename = "...")]` variants; `Event::is_private` gates broadcast filtering for `ClipboardHistory`, `NotificationAdd`, `NotificationHistory`, `SessionRestore`, `AiAuth`, `AiUsage`), config/snapshot payload structs (`Config`, `OutputInfo`, `Notification`, `ViewAdded`, etc.).
- `geometry.rs`: `Box`, `PartialBox` (preserves `None` vs `Some(0)`), `Transform` (`"normal"`, `"90"`, `"flipped"`, etc.).
- `js.rs`: `shell_url_allowed` (`file:` empty/loopback authority + loopback `http(s)` unless `VIEWPORT_ALLOW_REMOTE_SHELL=1`), `string_literal` (escapes `\u2028`/`\u2029`), `dispatch()`, `BRIDGE_SHIM`, `SUBFRAME_GUARD`, `bridge_script()`.

## Invariants
- Adding/changing a `Request` or `Event` variant requires updating `is_known_type` in `lib.rs`, `docs/ipc.md` (checked by unit test), and `TYPES` in `crates/viewport/src/msg.rs` (checked by `msg.rs` source-walk test).
- `SessionLock`, `SessionUnlock`, and `ShellCommand` exist on both `Request` and `Event` (110 + 43 - 3 = 150 distinct wire names).
