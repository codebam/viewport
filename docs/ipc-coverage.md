# Control-socket IPC coverage

How every message on the two IPC transports is covered by headless
integration tests. The companion reference is `docs/ipc.md` (the protocol
tables) and `docs/ipc.md`'s two tables are the source of the enumeration;
this file is the audit of what is exercised end-to-end (a real compositor,
started headless, driven over the control socket) versus what is only parsed
(serde unit tests in `crates/viewport-ipc`) or only exercised on the shell
side (`tests/shell.test.js` stubs the DOM — it never starts a compositor).

## How the count was done

- **115 wire types**: 74 `Request` variants (`crates/viewport-ipc/src/request.rs`,
  shell → compositor) and 41 `Event` variants
  (`crates/viewport-ipc/src/event.rs`, compositor → shell). `shell.command`
  appears on both sides (request and event). The page bridge
  (`js.rs` / `BRIDGE_SHIM`) carries the same set as the UNIX socket, so
  "both transports" adds no new names.
- **Integration means headless `apply` + wire**: a test that starts
  a `--headless` compositor and sends the message over the control socket
  (`viewport msg` in `tests/*.test.sh`, raw lines in
  `crates/viewport/tests/*.rs` via `common`), then asserts the answer fields
  *and* an observable state change (the next query's reply, a subscribed
  event, a foreign-toplevel state, a file, or a log line). Pure serde
  round-trips and `shell.test.js` behaviour are useful but are not counted
  as integration here.
- **Existing before this audit** counted both harnesses: the shell suite
  (`tests/*.test.sh` via `scripts/integration.sh`) and the Rust suite
  (`crates/viewport/tests/control_socket.rs`, `msg_cli.rs`, `wallpaper.rs`,
  `background.rs`). The gaps below were the ones neither exercised.
- **New tests** are all shell tests sending via `viewport msg` (`-t` or
  `--raw`, which is still `viewport msg` through the compositor's own
  parser), following the existing conventions: private runtime/state dirs,
  socket read from the log, `check` helper, PID-only cleanup.

Two defects were found while covering the gaps and are fixed in `FIXES.md`
as entries 55 and 56. Each was first asserted as an expected failure
(`known_fail`), and the expected failures are now plain `check`s.

| # | Defect | Found by |
| --- | --- | --- |
| 55 | `config.gaps`/`config.border`: a partially invalid message (valid first field, negative second) reported the refusal but the valid half stayed applied, silently diverging from what the shell was last told | `config-output-ipc` |
| 56 | `viewport msg -t` omitted five request types (`view.opacity_rule`, `view.tearing`, `view.idle_inhibit`, `gesture.capture`, `ai.login`): they exited 2 "unknown message type" without sending anything, while `--raw` delivered them | `view-ipc`, `shell-session-ipc`, `services-ipc` |

`viewport msg` itself validates before sending, which shapes the error
paths below: parse-level failures (unknown type, missing field, wrong
type, field the message does not have) are refused at the prompt with exit 2
and never reach the compositor; handler-level refusals are exit 1 with
`"viewport msg: <context>: <reason>"` on stderr (the `error` event the
compositor answered with, read out from behind the barrier in
`msg.rs`). Nonexistent view/notification ids are silently ignored (exit 0)
everywhere — a stale id is a race, not a refusal — and the tests assert
acceptance plus liveness rather than an error.

## Shell → compositor (requests)

| Message | Existing integration (before) | New headless test | Result |
| --- | --- | --- | --- |
| `view.layout` | X11 only (`xwayland-warp-hint`, `xwayland-capture-overlay`: send + capture) | `view-ipc` (Wayland paint window: accept + placement log line) | pass |
| `view.visible` | — | `view-ipc` (accept + still listed, re-shown by layout) | pass |
| `view.fullscreen` | — (foreign protocol only) | `view-ipc` (foreign `fullscreen=1/0` + `window.fullscreen.set` replay) | pass |
| `view.maximized` | — (foreign protocol only) | `view-ipc` (foreign `maximized=1/0`) | pass |
| `view.minimized` | — (foreign protocol only) | `view-ipc` (foreign + replayed `minimized` flag + focus drop) | pass |
| `view.focus` | untrusted refusal only (`control_socket.rs`) | `view-ipc` (functional: `view.focused`) | pass |
| `view.close` | — (foreign `close()` is protocol, not socket) | `view-ipc` (`view.removed` + client exits) | pass |
| `view.opacity` | — | `view-ipc` (accept; render-only state; out-of-range clamped) | pass (acceptance) |
| `view.opacity_rule` | — | `view-ipc` (`-t` and `--raw`) | pass |
| `view.capture` | `capture.test.sh` (private grant survives retitle) | — (reused: nonexistent-id silent case in `view-ipc`) | pass (existing) |
| `view.tearing` | — | `view-ipc` (`-t` and `--raw`) | pass |
| `view.idle_inhibit` | — | `view-ipc` (`-t` and `--raw`) | pass |
| `view.query` | `control_socket`, `msg_cli`, `capture`, warp tests | helper in every new test | pass |
| `background.focus` | `background.rs` (log: keyboard moves) | — | pass (existing) |
| `shell.focus` | — | `shell-session-ipc` (`view.focused` 0) | pass |
| `screencast.rect` | — | `shell-session-ipc` (accept; same `set_shell_overlays` path as `shell.overlay`) | pass (acceptance) |
| `shell.overview` | — | `shell-session-ipc` (accept; window still listed) | pass (acceptance) |
| `session.save` | — | `shell-session-ipc` (round-trip via `session.query`) | pass |
| `session.query` | — | `shell-session-ipc` (`session.restore` fields) | pass |
| `session.lock` | `lock-builtin` (black screen + log) | `shell-session-ipc` (`session.lock` event fields) | pass |
| `session.lock.drawn` | `lock-builtin` | `shell-session-ipc` (stale dropped + current heard, log) | pass |
| `session.unlock` | `lock-builtin` (wrong password stays locked) | `shell-session-ipc` (stale dropped; wrong password → `session.lock.error`) | pass (valid-password unlock needs real credentials, see below) |
| `notification.action` | `notification-centre` | — | pass (existing) |
| `notification.dismiss` | — | `services-ipc` (no socket close; history keeps) | pass |
| `notification.expire` | `notification-centre` | `services-ipc` (no socket close; history keeps) | pass |
| `notification.list` | `control_socket` (private) + `notification-centre` | helper (`history_now`) | pass |
| `notification.forget` | `notification-centre` | — | pass (existing) |
| `tray.activate` | `tray.test.sh` | — | pass (existing) |
| `clipboard.query` | `control_socket` (private) + `clipboard.test.sh` | — | pass (existing) |
| `clipboard.paste` | `clipboard.test.sh` | — | pass (existing) |
| `clipboard.forget` | `clipboard.test.sh` | — | pass (existing) |
| `launcher.query` | — | `config-output-ipc` (`launcher.list` fields, filter, generation) | pass |
| `launcher.launch` | — | `config-output-ipc` (stale/bad-id refusals + Foot launch → new view) | pass |
| `mpris.control` | `mpris.test.sh` (play-pause/next + unknown no-op) | — | pass (existing) |
| `ai.login` | — | `services-ipc` (bogus provider → `error`, over `-t` and `--raw`). The `openai` flow would reach the network, so it is not started. | pass (error path) |
| `power.profile` | — | `services-ipc` (unadvertised name dropped, acceptance). Standard names would switch the host profile, so they are not sent. | pass (acceptance) |
| `power.action` | — | `services-ipc` (invalid verb refused). Valid verbs go to logind on the system bus and would suspend/reboot/power off the machine, so they are never sent. | pass (error path) |
| `osk.key` | — | `services-ipc` (down/up acceptance) | pass (acceptance) |
| `network.scan` | — | `services-ipc` (`network.update` fields) | pass |
| `network.connect` | — | `services-ipc` (unknown ssid acceptance); missing ssid usage error | pass |
| `network.disconnect` | — | — (would leave the operator's network; no fields, so no error path either) | **not tested (safety)** |
| `network.radio` | — | — (would switch off the operator's radio) | **not tested (safety)** |
| `bluetooth.power` | — | — (would power down the operator's adapter) | **not tested (safety)** |
| `bluetooth.scan` | — | `services-ipc` (`bluetooth.update` fields) | pass |
| `bluetooth.device` | — | `services-ipc` (unknown action ignored, acceptance) | pass |
| `tray.menu.click` | `tray.test.sh` | — | pass (existing) |
| `tray.menu.closed` | `tray.test.sh` | — | pass (existing) |
| `tray.scroll` | `tray.test.sh` | — | pass (existing) |
| `shell.overlay` | `xwayland-capture-overlay` (hit-test) | — (same path as `screencast.rect`, which is covered) | pass (existing) |
| `workspace.list` | `xkcd workspace.test.sh` (`workspace.test.sh`) | — | pass (existing) |
| `output.configure` | `control_socket` (mirror, scale, revert) | — | pass (existing) |
| `output.hdr` | `msg_cli` (unknown output) | `config-output-ipc` (unknown + headless `HDR needs the drm backend`). Success needs the drm backend + capable head, so headless can only refuse. | pass (error paths) |
| `output.confirm` | `control_socket` | — | pass (existing) |
| `output.revert` | `control_socket` | — | pass (existing) |
| `output.active` | — | `config-output-ipc` (active flags; unknown refused) | pass |
| `gesture.capture` | — | `shell-session-ipc` (`-t` with and without `--gestures`, and `--raw`). Takes effect only at the next real `gesture.begin`. | pass |
| `output.query` | `control_socket`, `output-order`, `msg_cli` | helper | pass |
| `output.test_add` | `control_socket`, `output-order` | setup in `config-output-ipc` | pass |
| `output.test_remove` | `control_socket` (mirror, churn) | `config-output-ipc` (remove + unknown) | pass |
| `bind.add` | — | `config-output-ipc` (keymap announcement; invalid refused; replace) | pass |
| `shell.command` | `control_socket`, `msg_cli` | — | pass (existing) |
| `shell.exec` | — | `config-output-ipc` (file created on host) | pass |
| `status.refresh` | — | `config-output-ipc` (acceptance) | pass (acceptance) |
| `status.volume` | — | `config-output-ipc` (delta 0 acceptance; unknown target refused; no real volume change is asserted) | pass |
| `input.pointer` | warp/overlay tests | — | pass (existing) |
| `input.button` | `xwayland-capture-overlay` | — | pass (existing) |
| `input.key` | — | `view-ipc` (press/release acceptance) | pass (acceptance) |
| `input.scroll` | — | `view-ipc` (notch + empty no-op) | pass (acceptance) |
| `quit` | `control_socket`, `msg_cli` | — | pass (existing) |
| `config.gaps` | `control_socket` (persistence only) | `config-output-ipc` (next-config fields; atomic refusal) | pass |
| `config.border` | — | `config-output-ipc` (next-config fields; atomic refusal) | pass |
| `config.wallpaper` | `wallpaper.rs` | `config-output-ipc` (shell parity: fields, atomic refusals, removal) | pass |
| `config.dark_mode` | `control_socket` + `appearance.test.sh` | — | pass (existing) |
| `config.save` | `control_socket` (persistence) | `config-output-ipc` (`config.saved` path, overlay contents, config untouched) | pass |

Error paths are asserted alongside the success paths above (not as a
separate suite): unknown type over `-t` and over `--raw` (usage errors,
exit 2), missing fields, wrongly-typed fields, fields a message does not
have, handler refusals (exit 1 with the `error` context), and nonexistent
ids (silently ignored, exit 0, still listed/alive). The compositor-side
`error` event for malformed JSON / unknown type / non-string type is
already covered in `control_socket.rs`; it cannot be produced through
`viewport msg`, which refuses those before sending anything.

## Compositor → shell (events)

| Event | Existing integration (before) | New headless test | Result |
| --- | --- | --- | --- |
| `view.added` | `control_socket` (watchdog), `xwayland-float` (subscribe), `view.query` | `view-ipc` (fields) | pass |
| `view.removed` | — | `view-ipc` (after `view.close`) | pass |
| `view.props` | `shell.test.js` only | `view-ipc` (retitle → props + query agree) | pass |
| `view.parent` | — | — (needs an xdg-foreign dialog; neither test client creates one) | **not tested (client limit)** |
| `view.configured` | — | `view-ipc` (Foot 1x1 → 6x13; in-minimum silence) | pass |
| `view.focused` | — | `view-ipc` + `shell-session-ipc` | pass |
| `workspace.request` | `workspace.test.sh` | — | pass (existing) |
| `config` | `control_socket`, `msg_cli` | (`view.query` helper) | pass |
| `config.saved` | `control_socket` | `config-output-ipc` | pass |
| `modifiers` | — | — (physical-key path only under `bar: "auto"`; `inject_key` does not emit it, so there is no socket way to produce one) | **not tested (synthetic-path limit)** |
| `session.restore` | — | `shell-session-ipc` | pass |
| `session.lock` | `lock-builtin` (pixels + log) | `shell-session-ipc` (socket fields) | pass |
| `session.lock.error` | `lock-builtin` (stays locked, log) | `shell-session-ipc` (socket event + generation) | pass |
| `session.unlock` | `lock-takeover` (external locker) | — (a correct password needs real credentials) | **not tested (credentials)** |
| `notification.add` | — (centre reads history, not `add`) | `services-ipc` | pass |
| `notification.close` | — | `services-ipc` (sender withdrawal only; dismiss/expire emit no socket close) | pass |
| `notification.history` | `notification-centre` | helper | pass |
| `tray.update` | `tray.test.sh` | — | pass (existing) |
| `tray.menu` | `tray.test.sh` | — | pass (existing) |
| `clipboard.history` | `clipboard.test.sh` | — | pass (existing) |
| `launcher.list` | — | `config-output-ipc` | pass |
| `mpris.update` | `mpris.test.sh` | — | pass (existing) |
| `power.update` | — | — (worker is change-coalesced and the default snapshot *is* the empty one, so battery-less machines suppress it forever; no test could assert fields without battery hardware) | **not tested (hardware)** |
| `osk.wanted` | — | — (needs `zwp_text_input_v3` + touch + `auto`; headless settles `wanted: false` and would only assert the false edge) | **not tested (hardware)** |
| `network.update` | — | `services-ipc` | pass |
| `bluetooth.update` | — | `services-ipc` | pass |
| `status.update` | `control_socket`, `msg_cli` | (`status.refresh` acceptance) | pass (existing) |
| `ai.usage` | — | — (needs configured provider credentials) | **not tested (credentials)** |
| `ai.auth` | — | — (needs a real OAuth flow; `ai.login` refusal is covered) | **not tested (credentials)** |
| `output.layout` | `control_socket`, `output-order` | helper | pass |
| `shell.command` (event) | `control_socket`, `msg_cli` | (`window.fullscreen.set` replay in `view-ipc`) | pass |
| `gesture.begin`/`gesture.update`/`gesture.end` | — | — (need a real touchpad; `gesture.capture` only declares ownership) | **not tested (hardware)** |
| `screencast.pick`/`screencast.pick.done` | — | — (need a portal chooser drawn by a shell; the restore suite drives the portal over D-Bus, not these socket events) | **not tested (chooser)** |
| `screencast.active` | — | `shell-session-ipc` (replay: `active: false`) | pass |
| `shortcuts.pick`/`shortcuts.pick.done` | — | — (need the portal + a chooser dialogue; see `global_shortcuts.rs` for the bus half) | **not tested (chooser)** |
| `shell.reload` | — | — (only from `--watch-shell` file changes / reconnect; `input.key` cannot reach the configured `reload` binding, so there is no socket trigger) | **not tested (trigger limit)** |
| `error` | `control_socket` (malformed, unknown, bad type) + `msg_cli` (`--raw`) | new error paths via `viewport msg` (exit 2 / exit 1 strings) | pass |

## Documentation gaps closed alongside

`docs/ipc.md` was missing rows for `ai.login`, `shell.exec`,
`status.refresh`, `view.tearing`, `view.idle_inhibit` (shell →
compositor) and `ai.auth`, `ai.usage`, `shell.reload` (compositor →
shell); they have been added. `view.opacity_rule`, `view.tearing`,
`view.idle_inhibit`, `gesture.capture` and `ai.login` were documented
but not offerable by `viewport msg -t` (FIXES 56, now fixed).
